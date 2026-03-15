//! QUIC ALPN protocol handler for mesh enrollment.
//!
//! Implements `iroh::protocol::ProtocolHandler` for the `peat/enroll/1` ALPN,
//! allowing unenrolled nodes to request membership certificates over a QUIC
//! connection.
//!
//! # Two-Phase Connection Model
//!
//! ```text
//! Layer 0: QUIC transport (formation_secret → EndpointId)
//! Layer 1: peat/enroll/1 ALPN ← this handler (no certificate required)
//! Layer 2: cap/automerge/1 ALPN (certificate required)
//! ```
//!
//! A new node connects via Layer 0 (proving it knows the formation secret),
//! then opens a bidirectional stream on the enrollment ALPN. The authority
//! validates the bootstrap token and returns a signed certificate.
//!
//! # Wire Protocol
//!
//! ```text
//! Client → Server:
//!   [request_len:4 LE][EnrollmentRequest bytes]
//!
//! Server → Client:
//!   [response_len:4 LE][EnrollmentResponse bytes]
//! ```

use std::sync::Arc;

use iroh::endpoint::Connection;
use iroh::protocol::AcceptError;
use tracing::{info, warn};

use crate::security::enrollment::{EnrollmentRequest, EnrollmentResponse, EnrollmentService};

/// ALPN protocol identifier for Peat mesh enrollment.
pub const CAP_ENROLLMENT_ALPN: &[u8] = b"peat/enroll/1";

/// Maximum enrollment request size (64 KiB). Prevents resource exhaustion
/// from oversized payloads on the unauthenticated enrollment channel.
const MAX_REQUEST_SIZE: usize = 65536;

/// Maximum enrollment response size (64 KiB).
const MAX_RESPONSE_SIZE: usize = 65536;

/// QUIC protocol handler for mesh enrollment requests.
///
/// Registered with the Iroh Router alongside the sync and blob protocols.
/// Accepts connections on the `peat/enroll/1` ALPN and delegates to an
/// [`EnrollmentService`] implementation.
pub struct EnrollmentProtocolHandler {
    service: Arc<dyn EnrollmentService>,
}

impl std::fmt::Debug for EnrollmentProtocolHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollmentProtocolHandler").finish()
    }
}

impl EnrollmentProtocolHandler {
    /// Create a new enrollment protocol handler.
    pub fn new(service: Arc<dyn EnrollmentService>) -> Self {
        Self { service }
    }
}

impl iroh::protocol::ProtocolHandler for EnrollmentProtocolHandler {
    /// Handle an incoming QUIC connection on the `peat/enroll/1` ALPN.
    ///
    /// Accepts a single bidirectional stream, reads an [`EnrollmentRequest`],
    /// processes it through the enrollment service, and sends back an
    /// [`EnrollmentResponse`].
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id();
        info!(peer = %peer.fmt_short(), "enrollment connection received");

        let result = handle_enrollment(&connection, &*self.service).await;

        match result {
            Ok(approved) => {
                if approved {
                    info!(peer = %peer.fmt_short(), "enrollment approved");
                } else {
                    info!(peer = %peer.fmt_short(), "enrollment not approved");
                }
            }
            Err(e) => {
                warn!(
                    peer = %peer.fmt_short(),
                    error = %e,
                    "enrollment protocol error"
                );
            }
        }

        Ok(())
    }
}

/// Run the enrollment protocol on a single connection.
///
/// Returns `Ok(true)` if the enrollment was approved, `Ok(false)` otherwise.
async fn handle_enrollment(
    connection: &Connection,
    service: &dyn EnrollmentService,
) -> anyhow::Result<bool> {
    let (mut send, mut recv) = connection.accept_bi().await?;

    // Read request length
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let req_len = u32::from_le_bytes(len_buf) as usize;

    if req_len > MAX_REQUEST_SIZE {
        let resp = EnrollmentResponse::denied(
            format!("request too large: {req_len} bytes (max {MAX_REQUEST_SIZE})"),
            now_ms(),
        );
        send_response(&mut send, &resp).await?;
        send.finish()?;
        return Ok(false);
    }

    // Read request body
    let mut req_buf = vec![0u8; req_len];
    recv.read_exact(&mut req_buf).await?;

    // Decode request
    let request = match EnrollmentRequest::decode(&req_buf) {
        Ok(req) => req,
        Err(e) => {
            let resp = EnrollmentResponse::denied(format!("malformed request: {e}"), now_ms());
            send_response(&mut send, &resp).await?;
            send.finish()?;
            return Ok(false);
        }
    };

    // Process through enrollment service
    let response = match service.process_request(&request).await {
        Ok(resp) => resp,
        Err(e) => EnrollmentResponse::denied(format!("enrollment error: {e}"), now_ms()),
    };

    let approved = response.status == crate::security::enrollment::EnrollmentStatus::Approved;

    // Send response
    send_response(&mut send, &response).await?;
    send.finish()?;

    Ok(approved)
}

/// Encode and send an EnrollmentResponse on a QUIC send stream.
async fn send_response(
    send: &mut iroh::endpoint::SendStream,
    response: &EnrollmentResponse,
) -> anyhow::Result<()> {
    let resp_bytes = response.encode();
    if resp_bytes.len() > MAX_RESPONSE_SIZE {
        anyhow::bail!(
            "response too large: {} bytes (max {MAX_RESPONSE_SIZE})",
            resp_bytes.len()
        );
    }
    send.write_all(&(resp_bytes.len() as u32).to_le_bytes())
        .await?;
    send.write_all(&resp_bytes).await?;
    Ok(())
}

/// Client-side enrollment: connect to an authority node and request enrollment.
///
/// Opens a bidirectional stream on an existing QUIC connection (which must
/// have been established with the `peat/enroll/1` ALPN), sends an
/// [`EnrollmentRequest`], and reads back an [`EnrollmentResponse`].
pub async fn request_enrollment(
    connection: &Connection,
    request: &EnrollmentRequest,
) -> anyhow::Result<EnrollmentResponse> {
    let (mut send, mut recv) = connection.open_bi().await?;

    // Send request
    let req_bytes = request.encode();
    send.write_all(&(req_bytes.len() as u32).to_le_bytes())
        .await?;
    send.write_all(&req_bytes).await?;
    send.finish()?;

    // Read response length
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_le_bytes(len_buf) as usize;

    if resp_len > MAX_RESPONSE_SIZE {
        anyhow::bail!("response too large: {resp_len} bytes (max {MAX_RESPONSE_SIZE})");
    }

    // Read response body
    let mut resp_buf = vec![0u8; resp_len];
    recv.read_exact(&mut resp_buf).await?;

    let response = EnrollmentResponse::decode(&resp_buf)
        .map_err(|e| anyhow::anyhow!("failed to decode enrollment response: {e}"))?;

    Ok(response)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enrollment_alpn_is_valid() {
        assert_eq!(CAP_ENROLLMENT_ALPN, b"peat/enroll/1");
        assert!(!CAP_ENROLLMENT_ALPN.is_empty());
        assert!(CAP_ENROLLMENT_ALPN.len() < 256);
        assert!(CAP_ENROLLMENT_ALPN.iter().all(|b| b.is_ascii()));
    }

    #[test]
    fn test_enrollment_alpn_contains_version() {
        let alpn_str = std::str::from_utf8(CAP_ENROLLMENT_ALPN).unwrap();
        assert!(
            alpn_str.contains("/1"),
            "ALPN should include version suffix"
        );
        assert!(
            alpn_str.starts_with("peat/"),
            "ALPN should start with peat/ prefix"
        );
    }

    #[test]
    fn test_enrollment_alpn_distinct_from_sync() {
        use crate::storage::sync_transport::CAP_AUTOMERGE_ALPN;
        assert_ne!(
            CAP_ENROLLMENT_ALPN, CAP_AUTOMERGE_ALPN,
            "Enrollment and sync ALPNs must be distinct"
        );
    }

    #[test]
    fn test_max_request_size_reasonable() {
        assert_eq!(MAX_REQUEST_SIZE, 65536);
        assert!(
            MAX_REQUEST_SIZE >= 1024,
            "Request limit should allow reasonable payloads"
        );
        assert!(
            MAX_REQUEST_SIZE <= 1_048_576,
            "Request limit should prevent abuse"
        );
    }

    #[test]
    fn test_max_response_size_reasonable() {
        assert_eq!(MAX_RESPONSE_SIZE, 65536);
        assert_eq!(
            MAX_REQUEST_SIZE, MAX_RESPONSE_SIZE,
            "Request and response limits should match"
        );
    }

    #[test]
    fn test_now_ms_returns_plausible_timestamp() {
        let ts = now_ms();
        // Should be after 2020-01-01 (1577836800000 ms)
        assert!(ts > 1_577_836_800_000, "Timestamp should be after 2020");
        // Should be before 2100-01-01 (4102444800000 ms)
        assert!(ts < 4_102_444_800_000, "Timestamp should be before 2100");
    }

    #[test]
    fn test_now_ms_is_monotonic() {
        let t1 = now_ms();
        let t2 = now_ms();
        assert!(t2 >= t1, "Sequential timestamps should be non-decreasing");
    }

    /// `EnrollmentProtocolHandler` and the protocol flow require a running
    /// Iroh endpoint with QUIC connections. These are tested at the
    /// integration level (tests/hierarchy_e2e.rs). The unit tests here
    /// cover constants, the `now_ms` helper, and the Debug impl.
    #[test]
    fn test_handler_debug_impl() {
        // Verify the Debug impl exists and doesn't panic.
        // We can't construct EnrollmentProtocolHandler without an EnrollmentService
        // implementation, but we verify the struct definition compiles with Debug.
        let _: fn(&EnrollmentProtocolHandler, &mut std::fmt::Formatter) -> std::fmt::Result =
            <EnrollmentProtocolHandler as std::fmt::Debug>::fmt;
    }
}
