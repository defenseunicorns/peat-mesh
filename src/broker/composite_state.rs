//! Composite broker state that delegates mesh queries to an inner state
//! and document queries to a `StoreBrokerAdapter`.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::broadcast;

use super::state::{
    MeshBrokerState, MeshEvent, MeshNodeInfo, PeerSummary, ReadinessResponse, TopologySummary,
};
use super::store_adapter::StoreBrokerAdapter;

/// Combines a `MeshBrokerState` (for node/peer/topology data) with a
/// `StoreBrokerAdapter` (for document data from `AutomergeStore`).
///
/// This allows the broker HTTP endpoints to return actual document content
/// from the store instead of the default `None`.
pub struct CompositeBrokerState {
    inner: Arc<dyn MeshBrokerState>,
    store_adapter: StoreBrokerAdapter,
}

impl CompositeBrokerState {
    /// Create a new composite state.
    pub fn new(inner: Arc<dyn MeshBrokerState>, store_adapter: StoreBrokerAdapter) -> Self {
        Self {
            inner,
            store_adapter,
        }
    }
}

#[async_trait::async_trait]
impl MeshBrokerState for CompositeBrokerState {
    fn node_info(&self) -> MeshNodeInfo {
        self.inner.node_info()
    }

    async fn list_peers(&self) -> Vec<PeerSummary> {
        self.inner.list_peers().await
    }

    async fn get_peer(&self, id: &str) -> Option<PeerSummary> {
        self.inner.get_peer(id).await
    }

    fn topology(&self) -> TopologySummary {
        self.inner.topology()
    }

    fn subscribe_events(&self) -> broadcast::Receiver<MeshEvent> {
        self.inner.subscribe_events()
    }

    fn readiness(&self) -> ReadinessResponse {
        self.inner.readiness()
    }

    async fn list_documents(&self, collection: &str) -> Option<Vec<Value>> {
        self.store_adapter.list_documents(collection)
    }

    async fn get_document(&self, collection: &str, id: &str) -> Option<Value> {
        self.store_adapter.get_document(collection, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::json_convert::json_to_automerge;
    use crate::storage::AutomergeStore;

    struct MockState {
        tx: broadcast::Sender<MeshEvent>,
    }

    #[async_trait::async_trait]
    impl MeshBrokerState for MockState {
        fn node_info(&self) -> MeshNodeInfo {
            MeshNodeInfo {
                node_id: "test-node".into(),
                uptime_secs: 0,
                version: "0.0.0".into(),
            }
        }
        async fn list_peers(&self) -> Vec<PeerSummary> {
            vec![]
        }
        async fn get_peer(&self, _id: &str) -> Option<PeerSummary> {
            None
        }
        fn topology(&self) -> TopologySummary {
            TopologySummary {
                peer_count: 0,
                role: "standalone".into(),
                hierarchy_level: 0,
            }
        }
        fn subscribe_events(&self) -> broadcast::Receiver<MeshEvent> {
            self.tx.subscribe()
        }
    }

    fn create_composite(
        store: Arc<AutomergeStore>,
    ) -> (CompositeBrokerState, broadcast::Sender<MeshEvent>) {
        let (tx, _) = broadcast::channel(16);
        let mock = Arc::new(MockState { tx: tx.clone() });
        let adapter = StoreBrokerAdapter::new(store);
        (CompositeBrokerState::new(mock, adapter), tx)
    }

    #[tokio::test]
    async fn test_delegates_to_inner() {
        let store = Arc::new(AutomergeStore::in_memory());
        let (state, _tx) = create_composite(store);

        assert_eq!(state.node_info().node_id, "test-node");
        assert!(state.list_peers().await.is_empty());
        assert!(state.get_peer("any").await.is_none());
        assert_eq!(state.topology().peer_count, 0);
    }

    #[tokio::test]
    async fn test_documents_from_store() {
        let store = Arc::new(AutomergeStore::in_memory());

        let json = serde_json::json!({"name": "test-doc"});
        let am_doc = json_to_automerge(&json, None).unwrap();
        store.put("col:doc1", &am_doc).unwrap();

        let (state, _tx) = create_composite(store);

        let docs = state.list_documents("col").await.unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["_id"], "doc1");
        assert_eq!(docs[0]["name"], "test-doc");

        let doc = state.get_document("col", "doc1").await.unwrap();
        assert_eq!(doc["name"], "test-doc");
    }

    #[tokio::test]
    async fn test_missing_collection_returns_none() {
        let store = Arc::new(AutomergeStore::in_memory());
        let (state, _tx) = create_composite(store);

        assert!(state.list_documents("empty").await.is_none());
        assert!(state.get_document("empty", "id").await.is_none());
    }
}
