//! Transport and protocol handler for peat-mesh-node Automerge sync.
//!
//! Provides two main types:
//!
//! - [`MeshSyncTransport`]: implements [`SyncTransport`] by tracking active
//!   QUIC connections in a concurrent map.
//! - [`SyncProtocolHandler`]: implements `iroh::protocol::ProtocolHandler` so
//!   the Iroh Router dispatches incoming `CAP_AUTOMERGE_ALPN` connections to
//!   the sync coordinator.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use iroh::endpoint::Connection;
use iroh::protocol::AcceptError;
use iroh::{Endpoint, EndpointId};
use tracing::{debug, info, warn};

use super::automerge_sync::AutomergeSyncCoordinator;
use super::sync_transport::SyncTransport;
use crate::security::formation_key::{
    FormationAuthResult, FormationChallenge, FormationChallengeResponse, FormationKey,
};

/// Timeout for formation key authentication handshake reads and writes.
const FORMATION_AUTH_TIMEOUT: Duration = Duration::from_secs(30);

// ────────────────────────────────────────────────────────────────────────────
// MeshSyncTransport
// ────────────────────────────────────────────────────────────────────────────

/// Lightweight transport for peat-mesh-node that tracks QUIC connections
/// to peers and hands them to the sync coordinator on demand.
pub struct MeshSyncTransport {
    endpoint: Endpoint,
    connections: RwLock<HashMap<EndpointId, Connection>>,
}

impl fmt::Debug for MeshSyncTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let peer_count = self.connections.read().map(|c| c.len()).unwrap_or(0);
        f.debug_struct("MeshSyncTransport")
            .field(
                "endpoint_id",
                &format_args!("{}", self.endpoint.id().fmt_short()),
            )
            .field("connected_peers", &peer_count)
            .finish()
    }
}

impl MeshSyncTransport {
    /// Create a new transport sharing the given Iroh endpoint.
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            connections: RwLock::new(HashMap::new()),
        }
    }

    /// Store (or replace) the connection for a peer.
    pub fn insert_connection(&self, peer_id: EndpointId, conn: Connection) {
        let mut conns = self.connections.write().unwrap_or_else(|e| e.into_inner());
        conns.insert(peer_id, conn);
    }

    /// Remove the connection for a peer.
    pub fn remove_connection(&self, peer_id: &EndpointId) {
        let mut conns = self.connections.write().unwrap_or_else(|e| e.into_inner());
        conns.remove(peer_id);
    }

    /// Start full-duplex sync on a connection.
    ///
    /// Stores the connection and spawns a background task that accepts
    /// incoming bidirectional streams, forwarding each to the sync
    /// coordinator. This makes the QUIC connection fully bidirectional
    /// for sync — both sides can initiate streams.
    ///
    /// Use this for **both** incoming connections (from `SyncProtocolHandler`)
    /// and outgoing connections (from `connect_peer` or discovery).
    pub fn start_sync_connection(
        self: &Arc<Self>,
        connection: Connection,
        coordinator: Arc<AutomergeSyncCoordinator>,
    ) {
        let peer_id = connection.remote_id();
        self.insert_connection(peer_id, connection.clone());

        let transport = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match connection.accept_bi().await {
                    Ok((send, recv)) => {
                        debug!(
                            peer = %peer_id.fmt_short(),
                            "Accepted incoming sync stream"
                        );
                        let coord = coordinator.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                coord.handle_incoming_sync_stream(peer_id, send, recv).await
                            {
                                warn!(
                                    peer = %peer_id.fmt_short(),
                                    error = %e,
                                    "Error handling incoming sync stream"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        debug!(
                            peer = %peer_id.fmt_short(),
                            error = %e,
                            "Sync connection closed"
                        );
                        break;
                    }
                }
            }

            transport.remove_connection(&peer_id);
            coordinator.clear_peer_sync_state(peer_id);
            info!(
                peer = %peer_id.fmt_short(),
                "Cleaned up sync state for disconnected peer"
            );
        });
    }

    /// Get a reference to the underlying Iroh endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

#[async_trait]
impl SyncTransport for MeshSyncTransport {
    fn get_connection(&self, peer_id: &EndpointId) -> Option<Connection> {
        let conns = self.connections.read().unwrap_or_else(|e| e.into_inner());
        conns.get(peer_id).cloned()
    }

    fn connected_peers(&self) -> Vec<EndpointId> {
        let mut conns = self.connections.write().unwrap_or_else(|e| e.into_inner());
        // Prune closed connections while we enumerate.
        conns.retain(|_id, conn| conn.close_reason().is_none());
        conns.keys().copied().collect()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// SyncProtocolHandler
// ────────────────────────────────────────────────────────────────────────────

/// Iroh Router protocol handler for `CAP_AUTOMERGE_ALPN`.
///
/// When the Router receives a QUIC connection with our ALPN, it calls
/// [`accept`](iroh::protocol::ProtocolHandler::accept), which delegates to
/// [`MeshSyncTransport::start_sync_connection`] to set up full-duplex
/// sync on the connection.
///
/// ## Two-Phase Gating (Layer 2)
///
/// When a [`CertificateBundle`] is configured, incoming connections are
/// validated against it before sync begins. This is "Layer 2" of the
/// two-phase connection model:
///
/// - **Layer 0**: QUIC transport (formation_secret → EndpointId)
/// - **Layer 1**: `peat/enroll/1` ALPN (no cert required)
/// - **Layer 2**: `cap/automerge/1` ALPN — this handler (cert required when configured)
pub struct SyncProtocolHandler {
    transport: Arc<MeshSyncTransport>,
    coordinator: Arc<AutomergeSyncCoordinator>,
    /// Optional formation key for peer authentication.
    /// When set, incoming connections must pass HMAC challenge-response
    /// before sync streams are accepted.
    formation_key: Option<FormationKey>,
    /// Optional certificate bundle for Layer 2 peer validation.
    /// When set, peers must have a valid, non-expired certificate.
    certificate_bundle: Option<Arc<RwLock<crate::security::certificate::CertificateBundle>>>,
    /// Whether to hard-reject peers without certificates (vs. warn-and-allow).
    require_certificates: bool,
}

impl fmt::Debug for SyncProtocolHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncProtocolHandler")
            .field("has_formation_key", &self.formation_key.is_some())
            .field("has_certificate_bundle", &self.certificate_bundle.is_some())
            .field("require_certificates", &self.require_certificates)
            .finish()
    }
}

impl SyncProtocolHandler {
    pub fn new(
        transport: Arc<MeshSyncTransport>,
        coordinator: Arc<AutomergeSyncCoordinator>,
    ) -> Self {
        Self {
            transport,
            coordinator,
            formation_key: None,
            certificate_bundle: None,
            require_certificates: false,
        }
    }

    /// Create a handler with formation key authentication enabled.
    ///
    /// When a formation key is set, incoming connections must pass an
    /// HMAC-SHA256 challenge-response handshake before sync begins.
    /// Connections that fail authentication are rejected.
    pub fn with_formation_key(
        transport: Arc<MeshSyncTransport>,
        coordinator: Arc<AutomergeSyncCoordinator>,
        formation_key: FormationKey,
    ) -> Self {
        Self {
            transport,
            coordinator,
            formation_key: Some(formation_key),
            certificate_bundle: None,
            require_certificates: false,
        }
    }

    /// Enable Layer 2 certificate validation.
    ///
    /// When `require` is true, peers without a valid certificate in the bundle
    /// are rejected. When false, a warning is logged but the connection proceeds.
    pub fn with_certificate_bundle(
        mut self,
        bundle: Arc<RwLock<crate::security::certificate::CertificateBundle>>,
        require: bool,
    ) -> Self {
        self.certificate_bundle = Some(bundle);
        self.require_certificates = require;
        self
    }
}

impl iroh::protocol::ProtocolHandler for SyncProtocolHandler {
    /// Handle an incoming QUIC connection on the `CAP_AUTOMERGE_ALPN` ALPN.
    ///
    /// Authentication layers (in order):
    /// 1. **Certificate validation** (Layer 2): If a certificate bundle is
    ///    configured, the peer's EndpointId is checked against known certificates.
    /// 2. **Formation key** (Layer 0+): If configured, runs HMAC-SHA256
    ///    challenge-response handshake.
    ///
    /// Connections failing either check are rejected (or warned, depending on
    /// `require_certificates`).
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id();

        // Layer 2: Certificate validation
        if let Some(ref bundle) = self.certificate_bundle {
            let bundle = bundle.read().unwrap_or_else(|e| e.into_inner());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let peer_valid = bundle.validate_peer(peer.as_bytes(), now);

            if !peer_valid {
                if self.require_certificates {
                    warn!(
                        peer = %peer.fmt_short(),
                        "peer has no valid certificate, rejecting sync connection"
                    );
                    connection.close(2u32.into(), b"certificate required");
                    return Ok(());
                }
                debug!(
                    peer = %peer.fmt_short(),
                    "peer has no valid certificate (warn-and-allow mode)"
                );
            } else {
                debug!(peer = %peer.fmt_short(), "peer certificate validated");
            }
        }

        // Formation key authentication
        if let Some(ref fk) = self.formation_key {
            match Self::run_formation_auth(fk, &connection).await {
                Ok(()) => {
                    info!(peer = %peer.fmt_short(), "peer authenticated via formation key");
                }
                Err(e) => {
                    warn!(
                        peer = %peer.fmt_short(),
                        error = %e,
                        "peer failed formation key authentication, rejecting"
                    );
                    connection.close(1u32.into(), b"formation auth failed");
                    return Ok(());
                }
            }
        } else {
            info!(
                peer = %peer.fmt_short(),
                "accepted sync connection (no formation key configured)"
            );
        }

        self.transport
            .start_sync_connection(connection, self.coordinator.clone());

        Ok(())
    }
}

impl SyncProtocolHandler {
    /// Run formation key challenge-response on an accepted QUIC connection
    /// (acceptor/server side).
    ///
    /// Waits for the connecting peer to open a bidirectional auth stream,
    /// sends a challenge nonce, reads the HMAC response, and verifies it.
    /// All I/O operations are bounded by [`FORMATION_AUTH_TIMEOUT`].
    ///
    /// Returns `Ok(())` on successful authentication.
    async fn run_formation_auth(fk: &FormationKey, connection: &Connection) -> anyhow::Result<()> {
        // Acceptor waits for the connector to open the auth stream.
        let (mut send, mut recv) =
            tokio::time::timeout(FORMATION_AUTH_TIMEOUT, connection.accept_bi())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("formation auth timed out waiting for auth stream")
                })??;

        // Send challenge
        let (nonce, _expected) = fk.create_challenge();
        let challenge = FormationChallenge {
            formation_id: fk.formation_id().to_string(),
            nonce,
        };
        let challenge_bytes = challenge.to_bytes();
        tokio::time::timeout(FORMATION_AUTH_TIMEOUT, async {
            send.write_all(&(challenge_bytes.len() as u32).to_le_bytes())
                .await?;
            send.write_all(&challenge_bytes).await?;
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("formation auth timed out sending challenge"))??;

        // Read response
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(FORMATION_AUTH_TIMEOUT, recv.read_exact(&mut len_buf))
            .await
            .map_err(|_| anyhow::anyhow!("formation auth timed out reading response length"))??;
        let resp_len = u32::from_le_bytes(len_buf) as usize;
        if resp_len > 256 {
            anyhow::bail!("response too large: {resp_len}");
        }
        let mut resp_buf = vec![0u8; resp_len];
        tokio::time::timeout(FORMATION_AUTH_TIMEOUT, recv.read_exact(&mut resp_buf))
            .await
            .map_err(|_| anyhow::anyhow!("formation auth timed out reading response body"))??;

        let resp = FormationChallengeResponse::from_bytes(&resp_buf)
            .map_err(|e| anyhow::anyhow!("invalid response: {e}"))?;

        // Verify and send result
        if fk.verify_response(&nonce, &resp.response) {
            tokio::time::timeout(
                FORMATION_AUTH_TIMEOUT,
                send.write_all(&[FormationAuthResult::Accepted.to_byte()]),
            )
            .await
            .map_err(|_| anyhow::anyhow!("formation auth timed out sending accept"))??;
            send.finish()?;
            Ok(())
        } else {
            tokio::time::timeout(
                FORMATION_AUTH_TIMEOUT,
                send.write_all(&[FormationAuthResult::Rejected.to_byte()]),
            )
            .await
            .map_err(|_| anyhow::anyhow!("formation auth timed out sending reject"))??;
            send.finish()?;
            anyhow::bail!("HMAC verification failed")
        }
    }
}

/// Connector-side formation key authentication.
///
/// The connecting peer opens a bidirectional auth stream, reads the challenge
/// from the acceptor, computes the HMAC response, and reads back the
/// accept/reject verdict. All I/O is bounded by [`FORMATION_AUTH_TIMEOUT`].
///
/// Call this immediately after establishing a QUIC connection to a peer that
/// requires formation key authentication (before opening sync streams).
///
/// Returns `Ok(())` if the acceptor verified our response successfully.
pub async fn respond_to_formation_auth(
    fk: &FormationKey,
    connection: &Connection,
) -> anyhow::Result<()> {
    // Connector opens the auth stream.
    let (mut send, mut recv) = tokio::time::timeout(FORMATION_AUTH_TIMEOUT, connection.open_bi())
        .await
        .map_err(|_| anyhow::anyhow!("formation auth timed out opening auth stream"))??;

    // Read challenge from acceptor
    let mut len_buf = [0u8; 4];
    tokio::time::timeout(FORMATION_AUTH_TIMEOUT, recv.read_exact(&mut len_buf))
        .await
        .map_err(|_| anyhow::anyhow!("formation auth timed out reading challenge length"))??;
    let challenge_len = u32::from_le_bytes(len_buf) as usize;
    if challenge_len > 1024 {
        anyhow::bail!("challenge too large: {challenge_len}");
    }
    let mut challenge_buf = vec![0u8; challenge_len];
    tokio::time::timeout(FORMATION_AUTH_TIMEOUT, recv.read_exact(&mut challenge_buf))
        .await
        .map_err(|_| anyhow::anyhow!("formation auth timed out reading challenge body"))??;

    let challenge = FormationChallenge::from_bytes(&challenge_buf)
        .map_err(|e| anyhow::anyhow!("invalid challenge: {e}"))?;

    // Verify formation ID matches
    if challenge.formation_id != fk.formation_id() {
        anyhow::bail!(
            "formation ID mismatch: expected {}, got {}",
            fk.formation_id(),
            challenge.formation_id
        );
    }

    // Compute and send HMAC response
    let response = fk.respond_to_challenge(&challenge.nonce);
    let resp = FormationChallengeResponse { response };
    let resp_bytes = resp.to_bytes();
    tokio::time::timeout(FORMATION_AUTH_TIMEOUT, async {
        send.write_all(&(resp_bytes.len() as u32).to_le_bytes())
            .await?;
        send.write_all(&resp_bytes).await?;
        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("formation auth timed out sending response"))??;

    // Read accept/reject verdict
    let mut verdict = [0u8; 1];
    tokio::time::timeout(FORMATION_AUTH_TIMEOUT, recv.read_exact(&mut verdict))
        .await
        .map_err(|_| anyhow::anyhow!("formation auth timed out reading verdict"))??;

    match FormationAuthResult::from_byte(verdict[0]) {
        FormationAuthResult::Accepted => Ok(()),
        FormationAuthResult::Rejected => {
            anyhow::bail!("formation key authentication rejected by acceptor")
        }
    }
}
