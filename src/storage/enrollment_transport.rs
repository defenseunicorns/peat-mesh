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
