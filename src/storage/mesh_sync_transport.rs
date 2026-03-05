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

use async_trait::async_trait;
use iroh::endpoint::Connection;
use iroh::protocol::AcceptError;
use iroh::{Endpoint, EndpointId};
use tracing::{debug, info, warn};

use super::automerge_sync::AutomergeSyncCoordinator;
use super::sync_transport::SyncTransport;

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
        let mut conns = self.connections.write().unwrap();
        conns.insert(peer_id, conn);
    }

    /// Remove the connection for a peer.
    pub fn remove_connection(&self, peer_id: &EndpointId) {
        let mut conns = self.connections.write().unwrap();
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
                            if let Err(e) = coord
                                .handle_incoming_sync_stream(peer_id, send, recv)
                                .await
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
        let conns = self.connections.read().unwrap();
        conns.get(peer_id).cloned()
    }

    fn connected_peers(&self) -> Vec<EndpointId> {
        let mut conns = self.connections.write().unwrap();
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
pub struct SyncProtocolHandler {
    transport: Arc<MeshSyncTransport>,
    coordinator: Arc<AutomergeSyncCoordinator>,
}

impl fmt::Debug for SyncProtocolHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncProtocolHandler").finish()
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
        }
    }
}

impl iroh::protocol::ProtocolHandler for SyncProtocolHandler {
    /// Handle an incoming QUIC connection on the `CAP_AUTOMERGE_ALPN` ALPN.
    ///
    /// Delegates to [`MeshSyncTransport::start_sync_connection`] which stores
    /// the connection and spawns a full-duplex accept loop for sync streams.
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        info!(
            peer = %connection.remote_id().fmt_short(),
            "Accepted incoming sync connection"
        );

        self.transport
            .start_sync_connection(connection, self.coordinator.clone());

        Ok(())
    }
}
