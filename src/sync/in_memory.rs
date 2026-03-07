//! In-memory reference implementation of sync backend traits
//!
//! Provides `InMemoryBackend` — a zero-dependency implementation of all four
//! sync traits (`DocumentStore`, `PeerDiscovery`, `SyncEngine`, `DataSyncBackend`).
//!
//! Designed for:
//! - Unit and integration tests (no external services needed)
//! - Prototyping and development
//! - Embedded scenarios where persistence is not required
//!
//! # Example
//!
//! ```ignore
//! use peat_mesh::sync::in_memory::InMemoryBackend;
//! use peat_mesh::sync::types::{Document, Query};
//! use peat_mesh::sync::traits::DocumentStore;
//! use std::collections::HashMap;
//!
//! let backend = InMemoryBackend::new_initialized();
//! let doc = Document::new(HashMap::from([
//!     ("name".to_string(), serde_json::json!("test")),
//! ]));
//! let id = backend.upsert("my_collection", doc).await.unwrap();
//! let results = backend.query("my_collection", &Query::All).await.unwrap();
//! assert_eq!(results.len(), 1);
//! ```

use crate::qos::{DeletionPolicy, Tombstone};
use crate::sync::traits::*;
use crate::sync::types::*;
use anyhow::{bail, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, RwLock};

// =============================================================================
// Query Evaluator
// =============================================================================

/// Evaluate whether a document matches a query
///
/// Supports all Query variants including spatial queries.
pub fn evaluate_query(doc: &Document, query: &Query) -> bool {
    match query {
        Query::All => true,

        Query::Eq { field, value } => {
            if field == "id" {
                doc.id.as_deref() == value.as_str()
            } else {
                doc.fields.get(field.as_str()) == Some(value)
            }
        }

        Query::Lt { field, value } => {
            if let Some(doc_val) = doc.fields.get(field.as_str()) {
                compare_values(doc_val, value) == Some(std::cmp::Ordering::Less)
            } else {
                false
            }
        }

        Query::Gt { field, value } => {
            if let Some(doc_val) = doc.fields.get(field.as_str()) {
                compare_values(doc_val, value) == Some(std::cmp::Ordering::Greater)
            } else {
                false
            }
        }

        Query::And(queries) => queries.iter().all(|q| evaluate_query(doc, q)),

        Query::Or(queries) => queries.iter().any(|q| evaluate_query(doc, q)),

        Query::Not(inner) => !evaluate_query(doc, inner),

        Query::WithinRadius {
            center,
            radius_meters,
            lat_field,
            lon_field,
        } => {
            let lat_key = lat_field.as_deref().unwrap_or("lat");
            let lon_key = lon_field.as_deref().unwrap_or("lon");

            if let (Some(lat_val), Some(lon_val)) =
                (doc.fields.get(lat_key), doc.fields.get(lon_key))
            {
                if let (Some(lat), Some(lon)) = (lat_val.as_f64(), lon_val.as_f64()) {
                    let point = GeoPoint::new(lat, lon);
                    point.within_radius(center, *radius_meters)
                } else {
                    false
                }
            } else {
                false
            }
        }

        Query::WithinBounds {
            min,
            max,
            lat_field,
            lon_field,
        } => {
            let lat_key = lat_field.as_deref().unwrap_or("lat");
            let lon_key = lon_field.as_deref().unwrap_or("lon");

            if let (Some(lat_val), Some(lon_val)) =
                (doc.fields.get(lat_key), doc.fields.get(lon_key))
            {
                if let (Some(lat), Some(lon)) = (lat_val.as_f64(), lon_val.as_f64()) {
                    let point = GeoPoint::new(lat, lon);
                    point.within_bounds(min, max)
                } else {
                    false
                }
            } else {
                false
            }
        }

        Query::IncludeDeleted(inner) => evaluate_query(doc, inner),

        Query::DeletedOnly => doc
            .fields
            .get("_deleted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),

        Query::Custom(_) => true, // Custom queries match all in-memory
    }
}

/// Compare two serde_json::Value instances for ordering
fn compare_values(a: &serde_json::Value, b: &serde_json::Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => {
            a.as_f64()?.partial_cmp(&b.as_f64()?)
        }
        (serde_json::Value::String(a), serde_json::Value::String(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

// =============================================================================
// Observer Entry
// =============================================================================

struct ObserverEntry {
    collection: String,
    #[allow(dead_code)]
    query: Query,
    sender: mpsc::UnboundedSender<ChangeEvent>,
}

// =============================================================================
// InMemoryBackend
// =============================================================================

/// In-memory implementation of all four sync traits
///
/// Thread-safe via Arc internals. Cloning shares the same state.
#[derive(Clone)]
pub struct InMemoryBackend {
    /// Two-level map: collection -> doc_id -> Document
    collections: Arc<RwLock<HashMap<String, HashMap<String, Document>>>>,
    /// Manual peer list
    peers: Arc<RwLock<HashMap<PeerId, PeerInfo>>>,
    /// Change notification observers
    observers: Arc<std::sync::Mutex<Vec<ObserverEntry>>>,
    /// Tombstone tracking
    tombstones: Arc<RwLock<HashMap<String, Tombstone>>>,
    /// Peer event callbacks
    #[allow(clippy::type_complexity)]
    peer_callbacks: Arc<std::sync::Mutex<Vec<Box<dyn Fn(PeerEvent) + Send + Sync>>>>,
    /// Backend config (set on initialize)
    config: Arc<RwLock<Option<BackendConfig>>>,
    /// Syncing flag
    syncing: Arc<RwLock<bool>>,
    /// Sync subscriptions (collection names)
    subscriptions: Arc<RwLock<Vec<String>>>,
    /// Initialized flag
    initialized: Arc<RwLock<bool>>,
    /// Deletion policies per collection
    deletion_policies: Arc<RwLock<HashMap<String, DeletionPolicy>>>,
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryBackend {
    /// Create a new uninitialized backend
    pub fn new() -> Self {
        Self {
            collections: Arc::new(RwLock::new(HashMap::new())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            observers: Arc::new(std::sync::Mutex::new(Vec::new())),
            tombstones: Arc::new(RwLock::new(HashMap::new())),
            peer_callbacks: Arc::new(std::sync::Mutex::new(Vec::new())),
            config: Arc::new(RwLock::new(None)),
            syncing: Arc::new(RwLock::new(false)),
            subscriptions: Arc::new(RwLock::new(Vec::new())),
            initialized: Arc::new(RwLock::new(false)),
            deletion_policies: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new backend that's immediately ready for use in tests
    pub fn new_initialized() -> Self {
        let backend = Self::new();
        // Mark as initialized synchronously using try_write
        *backend.initialized.try_write().unwrap() = true;
        backend
    }

    /// Get total document count across all collections
    pub async fn document_count(&self) -> usize {
        let collections = self.collections.read().await;
        collections.values().map(|c| c.len()).sum()
    }

    /// Get the number of collections
    pub async fn collection_count(&self) -> usize {
        let collections = self.collections.read().await;
        collections.len()
    }

    /// Clear all documents in a collection
    pub async fn clear_collection(&self, collection: &str) {
        let mut collections = self.collections.write().await;
        if let Some(col) = collections.get_mut(collection) {
            col.clear();
        }
    }

    /// Set deletion policy for a collection
    pub async fn set_deletion_policy(&self, collection: &str, policy: DeletionPolicy) {
        let mut policies = self.deletion_policies.write().await;
        policies.insert(collection.to_string(), policy);
    }

    /// Notify observers about a change
    fn notify_observers(&self, collection: &str, event: ChangeEvent) {
        let observers = self.observers.lock().unwrap_or_else(|e| e.into_inner());
        for entry in observers.iter() {
            if entry.collection == collection {
                let _ = entry.sender.send(event.clone());
            }
        }
    }
}

// =============================================================================
// DocumentStore Implementation
// =============================================================================

#[async_trait]
impl DocumentStore for InMemoryBackend {
    async fn upsert(&self, collection: &str, document: Document) -> Result<DocumentId> {
        let id = document
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let mut doc = document;
        doc.id = Some(id.clone());
        doc.updated_at = SystemTime::now();

        let mut collections = self.collections.write().await;
        let col = collections
            .entry(collection.to_string())
            .or_insert_with(HashMap::new);
        col.insert(id.clone(), doc.clone());

        // Notify observers
        self.notify_observers(
            collection,
            ChangeEvent::Updated {
                collection: collection.to_string(),
                document: doc,
            },
        );

        Ok(id)
    }

    async fn query(&self, collection: &str, query: &Query) -> Result<Vec<Document>> {
        let collections = self.collections.read().await;
        let col = match collections.get(collection) {
            Some(col) => col,
            None => return Ok(Vec::new()),
        };

        let results: Vec<Document> = col
            .values()
            .filter(|doc| {
                // Apply deletion state filter
                if !query.matches_deletion_state(doc) {
                    return false;
                }
                // Apply query filter
                let effective_query = query.inner_query();
                evaluate_query(doc, effective_query)
            })
            .cloned()
            .collect();

        Ok(results)
    }

    async fn remove(&self, collection: &str, doc_id: &DocumentId) -> Result<()> {
        let mut collections = self.collections.write().await;
        if let Some(col) = collections.get_mut(collection) {
            if col.remove(doc_id).is_some() {
                self.notify_observers(
                    collection,
                    ChangeEvent::Removed {
                        collection: collection.to_string(),
                        doc_id: doc_id.clone(),
                    },
                );
            }
        }
        Ok(())
    }

    fn observe(&self, collection: &str, query: &Query) -> Result<ChangeStream> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Send initial snapshot
        // We can't await here (sync fn), so we'll skip initial snapshot for simplicity.
        // In practice, callers should query first then observe for changes.

        let entry = ObserverEntry {
            collection: collection.to_string(),
            query: query.clone(),
            sender: tx,
        };

        self.observers.lock().unwrap_or_else(|e| e.into_inner()).push(entry);

        Ok(ChangeStream { receiver: rx })
    }

    async fn delete(
        &self,
        collection: &str,
        doc_id: &DocumentId,
        reason: Option<&str>,
    ) -> Result<crate::qos::DeleteResult> {
        let policy = self.deletion_policy(collection);

        match &policy {
            DeletionPolicy::Immutable => Ok(crate::qos::DeleteResult::immutable()),

            DeletionPolicy::SoftDelete { .. } => {
                // Mark document with _deleted=true
                let mut collections = self.collections.write().await;
                if let Some(col) = collections.get_mut(collection) {
                    if let Some(doc) = col.get_mut(doc_id) {
                        doc.fields
                            .insert("_deleted".to_string(), serde_json::json!(true));
                        doc.updated_at = SystemTime::now();

                        self.notify_observers(
                            collection,
                            ChangeEvent::Updated {
                                collection: collection.to_string(),
                                document: doc.clone(),
                            },
                        );
                    }
                }

                Ok(crate::qos::DeleteResult::soft_deleted(policy))
            }

            DeletionPolicy::Tombstone { tombstone_ttl, .. } => {
                // Remove document and create tombstone
                self.remove(collection, doc_id).await?;

                let tombstone = Tombstone {
                    document_id: doc_id.clone(),
                    collection: collection.to_string(),
                    deleted_at: SystemTime::now(),
                    deleted_by: "local".to_string(),
                    lamport: 1,
                    reason: reason.map(|r| r.to_string()),
                };

                let key = format!("{}:{}", collection, doc_id);
                let mut tombstones = self.tombstones.write().await;
                tombstones.insert(key, tombstone);

                let expires_at = SystemTime::now() + *tombstone_ttl;
                Ok(crate::qos::DeleteResult {
                    deleted: true,
                    tombstone_id: Some(doc_id.clone()),
                    expires_at: Some(expires_at),
                    policy,
                })
            }

            DeletionPolicy::ImplicitTTL { .. } => {
                // No-op for TTL-based deletion
                Ok(crate::qos::DeleteResult::soft_deleted(policy))
            }
        }
    }

    fn deletion_policy(&self, collection: &str) -> DeletionPolicy {
        // Try to read from configured policies (non-async)
        if let Ok(policies) = self.deletion_policies.try_read() {
            if let Some(policy) = policies.get(collection) {
                return policy.clone();
            }
        }
        DeletionPolicy::default()
    }

    async fn get_tombstones(&self, collection: &str) -> Result<Vec<Tombstone>> {
        let tombstones = self.tombstones.read().await;
        let results: Vec<Tombstone> = tombstones
            .values()
            .filter(|t| t.collection == collection)
            .cloned()
            .collect();
        Ok(results)
    }

    async fn apply_tombstone(&self, tombstone: &Tombstone) -> Result<()> {
        self.remove(&tombstone.collection, &tombstone.document_id)
            .await?;

        let key = format!("{}:{}", tombstone.collection, tombstone.document_id);
        let mut tombstones = self.tombstones.write().await;
        tombstones.insert(key, tombstone.clone());
        Ok(())
    }
}

// =============================================================================
// PeerDiscovery Implementation
// =============================================================================

#[async_trait]
impl PeerDiscovery for InMemoryBackend {
    async fn start(&self) -> Result<()> {
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn discovered_peers(&self) -> Result<Vec<PeerInfo>> {
        let peers = self.peers.read().await;
        Ok(peers.values().cloned().collect())
    }

    async fn add_peer(&self, address: &str, transport: TransportType) -> Result<()> {
        let peer_id = format!("peer-{}", address);
        let info = PeerInfo {
            peer_id: peer_id.clone(),
            address: Some(address.to_string()),
            transport,
            connected: true,
            last_seen: SystemTime::now(),
            metadata: HashMap::new(),
        };

        let mut peers = self.peers.write().await;
        peers.insert(peer_id, info.clone());

        // Notify callbacks
        let callbacks = self.peer_callbacks.lock().unwrap_or_else(|e| e.into_inner());
        for cb in callbacks.iter() {
            cb(PeerEvent::Connected(info.clone()));
        }

        Ok(())
    }

    async fn wait_for_peer(&self, peer_id: &PeerId, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(50);

        loop {
            {
                let peers = self.peers.read().await;
                if peers.contains_key(peer_id) {
                    return Ok(());
                }
            }

            if tokio::time::Instant::now() >= deadline {
                bail!("Timeout waiting for peer {}", peer_id);
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    fn on_peer_event(&self, callback: Box<dyn Fn(PeerEvent) + Send + Sync>) {
        let mut callbacks = self.peer_callbacks.lock().unwrap_or_else(|e| e.into_inner());
        callbacks.push(callback);
    }

    async fn get_peer_info(&self, peer_id: &PeerId) -> Result<Option<PeerInfo>> {
        let peers = self.peers.read().await;
        Ok(peers.get(peer_id).cloned())
    }
}

// =============================================================================
// SyncEngine Implementation
// =============================================================================

#[async_trait]
impl SyncEngine for InMemoryBackend {
    async fn start_sync(&self) -> Result<()> {
        let mut syncing = self.syncing.write().await;
        *syncing = true;
        Ok(())
    }

    async fn stop_sync(&self) -> Result<()> {
        let mut syncing = self.syncing.write().await;
        *syncing = false;
        Ok(())
    }

    async fn subscribe(&self, collection: &str, _query: &Query) -> Result<SyncSubscription> {
        let mut subs = self.subscriptions.write().await;
        subs.push(collection.to_string());
        Ok(SyncSubscription::new(collection, ()))
    }

    async fn is_syncing(&self) -> Result<bool> {
        let syncing = self.syncing.read().await;
        Ok(*syncing)
    }
}

// =============================================================================
// DataSyncBackend Implementation
// =============================================================================

#[async_trait]
impl DataSyncBackend for InMemoryBackend {
    async fn initialize(&self, config: BackendConfig) -> Result<()> {
        let mut cfg = self.config.write().await;
        *cfg = Some(config);
        let mut initialized = self.initialized.write().await;
        *initialized = true;
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        let mut syncing = self.syncing.write().await;
        *syncing = false;
        let mut initialized = self.initialized.write().await;
        *initialized = false;
        Ok(())
    }

    fn document_store(&self) -> Arc<dyn DocumentStore> {
        Arc::new(self.clone())
    }

    fn peer_discovery(&self) -> Arc<dyn PeerDiscovery> {
        Arc::new(self.clone())
    }

    fn sync_engine(&self) -> Arc<dyn SyncEngine> {
        Arc::new(self.clone())
    }

    async fn is_ready(&self) -> bool {
        *self.initialized.read().await
    }

    fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            name: "InMemory".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- DocumentStore: upsert ---

    #[tokio::test]
    async fn test_upsert_new_document() {
        let backend = InMemoryBackend::new_initialized();
        let doc = Document::new(HashMap::from([(
            "name".to_string(),
            serde_json::json!("test"),
        )]));

        let id = backend.upsert("col", doc).await.unwrap();
        assert!(!id.is_empty());

        let result = backend.get("col", &id).await.unwrap();
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().get("name"),
            Some(&serde_json::json!("test"))
        );
    }

    #[tokio::test]
    async fn test_upsert_update_existing() {
        let backend = InMemoryBackend::new_initialized();
        let doc = Document::with_id(
            "d1",
            HashMap::from([("v".to_string(), serde_json::json!(1))]),
        );
        backend.upsert("col", doc).await.unwrap();

        // Update
        let doc2 = Document::with_id(
            "d1",
            HashMap::from([("v".to_string(), serde_json::json!(2))]),
        );
        backend.upsert("col", doc2).await.unwrap();

        let result = backend
            .get("col", &"d1".to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.get("v"), Some(&serde_json::json!(2)));
    }

    #[tokio::test]
    async fn test_upsert_with_id() {
        let backend = InMemoryBackend::new_initialized();
        let doc = Document::with_id("my-id", HashMap::new());
        let id = backend.upsert("col", doc).await.unwrap();
        assert_eq!(id, "my-id");
    }

    // --- DocumentStore: query ---

    #[tokio::test]
    async fn test_query_all() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert("col", Document::with_id("a", HashMap::new()))
            .await
            .unwrap();
        backend
            .upsert("col", Document::with_id("b", HashMap::new()))
            .await
            .unwrap();

        let results = backend.query("col", &Query::All).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_query_eq() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d1",
                    HashMap::from([("status".to_string(), serde_json::json!("active"))]),
                ),
            )
            .await
            .unwrap();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d2",
                    HashMap::from([("status".to_string(), serde_json::json!("inactive"))]),
                ),
            )
            .await
            .unwrap();

        let results = backend
            .query(
                "col",
                &Query::Eq {
                    field: "status".to_string(),
                    value: serde_json::json!("active"),
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some("d1".to_string()));
    }

    #[tokio::test]
    async fn test_query_eq_by_id() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert("col", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();
        backend
            .upsert("col", Document::with_id("d2", HashMap::new()))
            .await
            .unwrap();

        let results = backend
            .query(
                "col",
                &Query::Eq {
                    field: "id".to_string(),
                    value: serde_json::json!("d1"),
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_query_lt() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d1",
                    HashMap::from([("score".to_string(), serde_json::json!(10))]),
                ),
            )
            .await
            .unwrap();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d2",
                    HashMap::from([("score".to_string(), serde_json::json!(20))]),
                ),
            )
            .await
            .unwrap();

        let results = backend
            .query(
                "col",
                &Query::Lt {
                    field: "score".to_string(),
                    value: serde_json::json!(15),
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some("d1".to_string()));
    }

    #[tokio::test]
    async fn test_query_gt() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d1",
                    HashMap::from([("score".to_string(), serde_json::json!(10))]),
                ),
            )
            .await
            .unwrap();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d2",
                    HashMap::from([("score".to_string(), serde_json::json!(20))]),
                ),
            )
            .await
            .unwrap();

        let results = backend
            .query(
                "col",
                &Query::Gt {
                    field: "score".to_string(),
                    value: serde_json::json!(15),
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some("d2".to_string()));
    }

    #[tokio::test]
    async fn test_query_and() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d1",
                    HashMap::from([
                        ("type".to_string(), serde_json::json!("sensor")),
                        ("score".to_string(), serde_json::json!(10)),
                    ]),
                ),
            )
            .await
            .unwrap();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d2",
                    HashMap::from([
                        ("type".to_string(), serde_json::json!("sensor")),
                        ("score".to_string(), serde_json::json!(20)),
                    ]),
                ),
            )
            .await
            .unwrap();

        let results = backend
            .query(
                "col",
                &Query::And(vec![
                    Query::Eq {
                        field: "type".to_string(),
                        value: serde_json::json!("sensor"),
                    },
                    Query::Gt {
                        field: "score".to_string(),
                        value: serde_json::json!(15),
                    },
                ]),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some("d2".to_string()));
    }

    #[tokio::test]
    async fn test_query_or() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d1",
                    HashMap::from([("status".to_string(), serde_json::json!("active"))]),
                ),
            )
            .await
            .unwrap();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d2",
                    HashMap::from([("status".to_string(), serde_json::json!("pending"))]),
                ),
            )
            .await
            .unwrap();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d3",
                    HashMap::from([("status".to_string(), serde_json::json!("inactive"))]),
                ),
            )
            .await
            .unwrap();

        let results = backend
            .query(
                "col",
                &Query::Or(vec![
                    Query::Eq {
                        field: "status".to_string(),
                        value: serde_json::json!("active"),
                    },
                    Query::Eq {
                        field: "status".to_string(),
                        value: serde_json::json!("pending"),
                    },
                ]),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_query_not() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d1",
                    HashMap::from([("status".to_string(), serde_json::json!("active"))]),
                ),
            )
            .await
            .unwrap();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d2",
                    HashMap::from([("status".to_string(), serde_json::json!("inactive"))]),
                ),
            )
            .await
            .unwrap();

        let results = backend
            .query(
                "col",
                &Query::Not(Box::new(Query::Eq {
                    field: "status".to_string(),
                    value: serde_json::json!("inactive"),
                })),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some("d1".to_string()));
    }

    #[tokio::test]
    async fn test_query_within_radius() {
        let backend = InMemoryBackend::new_initialized();
        // San Francisco
        backend
            .upsert(
                "beacons",
                Document::with_id(
                    "sf",
                    HashMap::from([
                        ("lat".to_string(), serde_json::json!(37.7749)),
                        ("lon".to_string(), serde_json::json!(-122.4194)),
                    ]),
                ),
            )
            .await
            .unwrap();
        // Los Angeles (far away)
        backend
            .upsert(
                "beacons",
                Document::with_id(
                    "la",
                    HashMap::from([
                        ("lat".to_string(), serde_json::json!(34.0522)),
                        ("lon".to_string(), serde_json::json!(-118.2437)),
                    ]),
                ),
            )
            .await
            .unwrap();

        let results = backend
            .query(
                "beacons",
                &Query::WithinRadius {
                    center: GeoPoint::new(37.78, -122.42),
                    radius_meters: 5000.0,
                    lat_field: None,
                    lon_field: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some("sf".to_string()));
    }

    #[tokio::test]
    async fn test_query_within_bounds() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert(
                "beacons",
                Document::with_id(
                    "in",
                    HashMap::from([
                        ("lat".to_string(), serde_json::json!(37.5)),
                        ("lon".to_string(), serde_json::json!(-122.5)),
                    ]),
                ),
            )
            .await
            .unwrap();
        backend
            .upsert(
                "beacons",
                Document::with_id(
                    "out",
                    HashMap::from([
                        ("lat".to_string(), serde_json::json!(40.0)),
                        ("lon".to_string(), serde_json::json!(-120.0)),
                    ]),
                ),
            )
            .await
            .unwrap();

        let results = backend
            .query(
                "beacons",
                &Query::WithinBounds {
                    min: GeoPoint::new(37.0, -123.0),
                    max: GeoPoint::new(38.0, -122.0),
                    lat_field: None,
                    lon_field: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some("in".to_string()));
    }

    #[tokio::test]
    async fn test_query_deletion_filters() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert(
                "col",
                Document::with_id(
                    "alive",
                    HashMap::from([("name".to_string(), serde_json::json!("alive"))]),
                ),
            )
            .await
            .unwrap();

        let mut deleted_fields = HashMap::new();
        deleted_fields.insert("name".to_string(), serde_json::json!("deleted"));
        deleted_fields.insert("_deleted".to_string(), serde_json::json!(true));
        backend
            .upsert("col", Document::with_id("dead", deleted_fields))
            .await
            .unwrap();

        // Normal query excludes deleted
        let results = backend.query("col", &Query::All).await.unwrap();
        assert_eq!(results.len(), 1);

        // IncludeDeleted shows all
        let results = backend
            .query("col", &Query::IncludeDeleted(Box::new(Query::All)))
            .await
            .unwrap();
        assert_eq!(results.len(), 2);

        // DeletedOnly shows only deleted
        let results = backend.query("col", &Query::DeletedOnly).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, Some("dead".to_string()));
    }

    // --- DocumentStore: remove ---

    #[tokio::test]
    async fn test_remove_document() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert("col", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();

        backend.remove("col", &"d1".to_string()).await.unwrap();
        let result = backend.get("col", &"d1".to_string()).await.unwrap();
        assert!(result.is_none());
    }

    // --- DocumentStore: get ---

    #[tokio::test]
    async fn test_get_nonexistent() {
        let backend = InMemoryBackend::new_initialized();
        let result = backend.get("col", &"missing".to_string()).await.unwrap();
        assert!(result.is_none());
    }

    // --- DocumentStore: count ---

    #[tokio::test]
    async fn test_count() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert("col", Document::with_id("a", HashMap::new()))
            .await
            .unwrap();
        backend
            .upsert("col", Document::with_id("b", HashMap::new()))
            .await
            .unwrap();

        let count = backend.count("col", &Query::All).await.unwrap();
        assert_eq!(count, 2);
    }

    // --- DocumentStore: observe ---

    #[tokio::test]
    async fn test_observe_updates() {
        let backend = InMemoryBackend::new_initialized();

        let mut stream = backend.observe("col", &Query::All).unwrap();

        // Insert a document
        backend
            .upsert(
                "col",
                Document::with_id(
                    "d1",
                    HashMap::from([("v".to_string(), serde_json::json!(1))]),
                ),
            )
            .await
            .unwrap();

        // Should receive update
        let event = stream.receiver.try_recv().unwrap();
        match event {
            ChangeEvent::Updated { document, .. } => {
                assert_eq!(document.id, Some("d1".to_string()));
            }
            _ => panic!("Expected Updated event"),
        }
    }

    #[tokio::test]
    async fn test_observe_removals() {
        let backend = InMemoryBackend::new_initialized();

        let mut stream = backend.observe("col", &Query::All).unwrap();

        backend
            .upsert("col", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();
        // Consume the update event
        let _ = stream.receiver.try_recv();

        // Remove
        backend.remove("col", &"d1".to_string()).await.unwrap();

        let event = stream.receiver.try_recv().unwrap();
        match event {
            ChangeEvent::Removed { doc_id, .. } => {
                assert_eq!(doc_id, "d1");
            }
            _ => panic!("Expected Removed event"),
        }
    }

    // --- DocumentStore: delete policies ---

    #[tokio::test]
    async fn test_delete_soft() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert("col", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();

        let result = backend
            .delete("col", &"d1".to_string(), None)
            .await
            .unwrap();
        assert!(result.deleted);

        // Document should still exist with _deleted=true
        let doc = backend.get("col", &"d1".to_string()).await.unwrap();
        // Default policy is SoftDelete, so doc gets _deleted flag
        // But get() uses Eq query which goes through our evaluate, not matches_deletion_state
        // Let's check via IncludeDeleted
        let results = backend
            .query("col", &Query::IncludeDeleted(Box::new(Query::All)))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].fields.get("_deleted"),
            Some(&serde_json::json!(true))
        );

        // Also verify: default `get` won't find soft-deleted doc since the default
        // query path uses Eq which doesn't apply deletion filter... but our get()
        // impl uses the default trait method which queries with Eq on "id" field.
        // Our query() applies deletion state filter, so soft-deleted docs are excluded.
        let doc = backend.get("col", &"d1".to_string()).await.unwrap();
        assert!(doc.is_none()); // Excluded by deletion filter
    }

    #[tokio::test]
    async fn test_delete_tombstone() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .set_deletion_policy(
                "col",
                DeletionPolicy::Tombstone {
                    tombstone_ttl: Duration::from_secs(3600),
                    delete_wins: true,
                },
            )
            .await;

        backend
            .upsert("col", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();

        let result = backend
            .delete("col", &"d1".to_string(), Some("test reason"))
            .await
            .unwrap();
        assert!(result.deleted);
        assert!(result.tombstone_id.is_some());

        // Document removed
        let doc = backend.get("col", &"d1".to_string()).await.unwrap();
        assert!(doc.is_none());

        // Tombstone exists
        let tombstones = backend.get_tombstones("col").await.unwrap();
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].document_id, "d1");
        assert_eq!(tombstones[0].reason, Some("test reason".to_string()));
    }

    #[tokio::test]
    async fn test_delete_immutable() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .set_deletion_policy("col", DeletionPolicy::Immutable)
            .await;

        backend
            .upsert("col", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();

        let result = backend
            .delete("col", &"d1".to_string(), None)
            .await
            .unwrap();
        assert!(!result.deleted);

        // Document still exists
        let doc = backend.get("col", &"d1".to_string()).await.unwrap();
        assert!(doc.is_some());
    }

    #[tokio::test]
    async fn test_tombstone_operations() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert("col", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();

        let tombstone = Tombstone {
            collection: "col".to_string(),
            document_id: "d1".to_string(),
            deleted_at: SystemTime::now(),
            deleted_by: "remote-node".to_string(),
            lamport: 5,
            reason: None,
        };

        backend.apply_tombstone(&tombstone).await.unwrap();

        let doc = backend.get("col", &"d1".to_string()).await.unwrap();
        assert!(doc.is_none());

        let tombstones = backend.get_tombstones("col").await.unwrap();
        assert_eq!(tombstones.len(), 1);
    }

    // --- PeerDiscovery ---

    #[tokio::test]
    async fn test_add_peer() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .add_peer("192.168.1.1:5000", TransportType::Tcp)
            .await
            .unwrap();

        let peers = backend.discovered_peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert!(peers[0].connected);
    }

    #[tokio::test]
    async fn test_wait_for_peer_success() {
        let backend = InMemoryBackend::new_initialized();

        // Add peer in background
        let backend_clone = backend.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            backend_clone
                .add_peer("10.0.0.1:5000", TransportType::Tcp)
                .await
                .unwrap();
        });

        backend
            .wait_for_peer(&"peer-10.0.0.1:5000".to_string(), Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_peer_timeout() {
        let backend = InMemoryBackend::new_initialized();
        let result = backend
            .wait_for_peer(&"missing-peer".to_string(), Duration::from_millis(100))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_peer_event_callbacks() {
        let backend = InMemoryBackend::new_initialized();
        let called = Arc::new(std::sync::Mutex::new(false));
        let called_clone = called.clone();

        backend.on_peer_event(Box::new(move |event| {
            if matches!(event, PeerEvent::Connected(_)) {
                *called_clone.lock().unwrap_or_else(|e| e.into_inner()) = true;
            }
        }));

        backend
            .add_peer("10.0.0.1:5000", TransportType::Tcp)
            .await
            .unwrap();

        assert!(*called.lock().unwrap_or_else(|e| e.into_inner()));
    }

    // --- SyncEngine ---

    #[tokio::test]
    async fn test_start_stop_sync() {
        let backend = InMemoryBackend::new_initialized();

        assert!(!backend.is_syncing().await.unwrap());

        backend.start_sync().await.unwrap();
        assert!(backend.is_syncing().await.unwrap());

        backend.stop_sync().await.unwrap();
        assert!(!backend.is_syncing().await.unwrap());
    }

    #[tokio::test]
    async fn test_subscribe() {
        let backend = InMemoryBackend::new_initialized();
        let sub = backend.subscribe("beacons", &Query::All).await.unwrap();
        assert_eq!(sub.collection(), "beacons");
    }

    #[tokio::test]
    async fn test_is_syncing() {
        let backend = InMemoryBackend::new_initialized();
        assert!(!backend.is_syncing().await.unwrap());
    }

    // --- DataSyncBackend ---

    #[tokio::test]
    async fn test_lifecycle() {
        let backend = InMemoryBackend::new();

        assert!(!backend.is_ready().await);

        backend
            .initialize(BackendConfig {
                app_id: "test".to_string(),
                persistence_dir: std::path::PathBuf::from("/tmp/test"),
                shared_key: None,
                transport: TransportConfig::default(),
                extra: HashMap::new(),
            })
            .await
            .unwrap();

        assert!(backend.is_ready().await);

        backend.shutdown().await.unwrap();
        assert!(!backend.is_ready().await);
    }

    #[tokio::test]
    async fn test_backend_info() {
        let backend = InMemoryBackend::new_initialized();
        let info = backend.backend_info();
        assert_eq!(info.name, "InMemory");
    }

    #[tokio::test]
    async fn test_is_ready() {
        let backend = InMemoryBackend::new();
        assert!(!backend.is_ready().await);

        let backend = InMemoryBackend::new_initialized();
        assert!(backend.is_ready().await);
    }

    #[tokio::test]
    async fn test_accessors() {
        let backend = InMemoryBackend::new_initialized();
        let _store = backend.document_store();
        let _disc = backend.peer_discovery();
        let _engine = backend.sync_engine();
    }

    #[test]
    fn test_as_any_downcast() {
        let backend = InMemoryBackend::new_initialized();
        let any = backend.as_any();
        assert!(any.downcast_ref::<InMemoryBackend>().is_some());
    }

    // --- Convenience API ---

    #[tokio::test]
    async fn test_document_count() {
        let backend = InMemoryBackend::new_initialized();
        assert_eq!(backend.document_count().await, 0);

        backend
            .upsert("a", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();
        backend
            .upsert("b", Document::with_id("d2", HashMap::new()))
            .await
            .unwrap();

        assert_eq!(backend.document_count().await, 2);
    }

    #[tokio::test]
    async fn test_collection_count() {
        let backend = InMemoryBackend::new_initialized();
        assert_eq!(backend.collection_count().await, 0);

        backend
            .upsert("a", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();
        backend
            .upsert("b", Document::with_id("d2", HashMap::new()))
            .await
            .unwrap();

        assert_eq!(backend.collection_count().await, 2);
    }

    #[tokio::test]
    async fn test_clear_collection() {
        let backend = InMemoryBackend::new_initialized();
        backend
            .upsert("col", Document::with_id("d1", HashMap::new()))
            .await
            .unwrap();
        backend
            .upsert("col", Document::with_id("d2", HashMap::new()))
            .await
            .unwrap();

        assert_eq!(backend.document_count().await, 2);
        backend.clear_collection("col").await;
        assert_eq!(backend.document_count().await, 0);
    }

    #[test]
    fn test_default_impl() {
        let _backend: InMemoryBackend = Default::default();
    }

    // --- Integration ---

    #[tokio::test]
    async fn test_full_workflow() {
        let backend = InMemoryBackend::new();

        // Initialize
        backend
            .initialize(BackendConfig {
                app_id: "integration-test".to_string(),
                persistence_dir: std::path::PathBuf::from("/tmp/test"),
                shared_key: None,
                transport: TransportConfig::default(),
                extra: HashMap::new(),
            })
            .await
            .unwrap();
        assert!(backend.is_ready().await);

        // Start sync
        backend.start_sync().await.unwrap();
        assert!(backend.is_syncing().await.unwrap());

        // Subscribe
        let _sub = backend.subscribe("beacons", &Query::All).await.unwrap();

        // Add peer
        backend
            .add_peer("192.168.1.1:5000", TransportType::Tcp)
            .await
            .unwrap();

        // CRUD operations via trait
        let store = backend.document_store();
        let id = store
            .upsert(
                "beacons",
                Document::new(HashMap::from([
                    ("lat".to_string(), serde_json::json!(37.7749)),
                    ("lon".to_string(), serde_json::json!(-122.4194)),
                    ("name".to_string(), serde_json::json!("SF HQ")),
                ])),
            )
            .await
            .unwrap();

        let results = store.query("beacons", &Query::All).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].get("name"), Some(&serde_json::json!("SF HQ")));

        // Spatial query
        let nearby = store
            .query(
                "beacons",
                &Query::WithinRadius {
                    center: GeoPoint::new(37.78, -122.42),
                    radius_meters: 5000.0,
                    lat_field: None,
                    lon_field: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(nearby.len(), 1);

        // Delete
        store
            .delete("beacons", &id, Some("decommissioned"))
            .await
            .unwrap();

        // Stop
        backend.stop_sync().await.unwrap();
        backend.shutdown().await.unwrap();
    }

    // --- Query evaluator unit tests ---

    #[test]
    fn test_evaluate_query_custom() {
        let doc = Document::with_id("d1", HashMap::new());
        assert!(evaluate_query(&doc, &Query::Custom("anything".to_string())));
    }

    #[test]
    fn test_evaluate_query_missing_field_lt() {
        let doc = Document::with_id("d1", HashMap::new());
        let query = Query::Lt {
            field: "missing".to_string(),
            value: serde_json::json!(10),
        };
        assert!(!evaluate_query(&doc, &query));
    }

    #[test]
    fn test_evaluate_query_missing_field_gt() {
        let doc = Document::with_id("d1", HashMap::new());
        let query = Query::Gt {
            field: "missing".to_string(),
            value: serde_json::json!(10),
        };
        assert!(!evaluate_query(&doc, &query));
    }

    #[test]
    fn test_compare_values_strings() {
        let a = serde_json::json!("apple");
        let b = serde_json::json!("banana");
        assert_eq!(compare_values(&a, &b), Some(std::cmp::Ordering::Less));
    }

    #[test]
    fn test_compare_values_incompatible() {
        let a = serde_json::json!("string");
        let b = serde_json::json!(42);
        assert_eq!(compare_values(&a, &b), None);
    }
}
