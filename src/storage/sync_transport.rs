//! Transport and routing abstractions for sync operations
//!
//! These traits decouple the sync coordinator from concrete transport
//! implementations (e.g., IrohTransport) and routing strategies (e.g.,
//! HierarchicalRouter), enabling reuse across eche-mesh and eche-mesh-node.

use async_trait::async_trait;
use iroh::endpoint::Connection;
use iroh::EndpointId;

use super::automerge_sync::SyncDirection;

/// ALPN protocol identifier for Eche Protocol Automerge sync
pub const CAP_AUTOMERGE_ALPN: &[u8] = b"cap/automerge/1";

/// Transport abstraction for sync operations
///
/// Provides the minimal connection management surface needed by
/// `AutomergeSyncCoordinator` and `SyncChannelManager`.
#[async_trait]
pub trait SyncTransport: Send + Sync + 'static {
    /// Get an existing connection to a peer, if one exists.
    fn get_connection(&self, peer_id: &EndpointId) -> Option<Connection>;

    /// List all currently connected peer IDs.
    fn connected_peers(&self) -> Vec<EndpointId>;
}

/// Routing abstraction for hierarchical sync direction
///
/// When present, the coordinator uses this to route sync messages based
/// on document direction (Upward, Downward, Lateral, Broadcast).
/// When absent, the coordinator broadcasts to all connected peers.
#[async_trait]
pub trait SyncRouter: Send + Sync + 'static {
    /// Get the peers to sync with for the given direction.
    ///
    /// `connected` is the current set of connected peers (from `SyncTransport::connected_peers()`).
    async fn get_targets(
        &self,
        direction: SyncDirection,
        connected: &[EndpointId],
    ) -> Vec<EndpointId>;

    /// Whether this node is the cell leader.
    async fn is_leader(&self) -> bool;
}
