//! Typed collection API for serde-based document access
//!
//! Provides `TypedCollection<T>` which wraps `AutomergeStore` with automatic
//! serde serialization/deserialization, integrated queries, and prefix-filtered
//! change subscriptions.

use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tokio_stream::StreamExt;

use super::automerge_store::AutomergeStore;
use super::json_convert::{automerge_to_json, json_to_automerge};
use super::query::Query;

/// A typed, serde-based view over a namespaced document collection.
///
/// `TypedCollection<T>` provides a higher-level API for working with
/// Automerge documents, handling serialization/deserialization automatically.
///
/// # Example
///
/// ```ignore
/// #[derive(Serialize, Deserialize)]
/// struct SensorReading { device: String, temperature_c: f64 }
///
/// let sensors = store.typed_collection::<SensorReading>("sensors");
/// sensors.upsert("reading-001", &SensorReading {
///     device: "sensor-42".into(),
///     temperature_c: 23.5,
/// })?;
///
/// let reading = sensors.get("reading-001")?.unwrap();
/// ```
pub struct TypedCollection<T> {
    store: Arc<AutomergeStore>,
    name: String,
    prefix: String,
    _marker: PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> TypedCollection<T> {
    /// Create a new typed collection view.
    pub fn new(store: Arc<AutomergeStore>, name: &str) -> Self {
        Self {
            store,
            name: name.to_string(),
            prefix: format!("{}:", name),
            _marker: PhantomData,
        }
    }

    /// Insert or update a document.
    ///
    /// Serializes `doc` to JSON, then converts to Automerge. If a document
    /// with the same ID already exists, it is forked (preserving CRDT history)
    /// before being updated.
    pub fn upsert(&self, id: &str, doc: &T) -> Result<()> {
        let json = serde_json::to_value(doc)?;
        let key = format!("{}{}", self.prefix, id);
        let existing = self.store.get(&key)?;
        let am_doc = json_to_automerge(&json, existing.as_ref())?;
        self.store.put(&key, &am_doc)?;
        Ok(())
    }

    /// Get a document by ID, deserialized into `T`.
    pub fn get(&self, id: &str) -> Result<Option<T>> {
        let key = format!("{}{}", self.prefix, id);
        match self.store.get(&key)? {
            Some(am_doc) => {
                let json = automerge_to_json(&am_doc);
                let val: T = serde_json::from_value(json)?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }

    /// Get a document by ID as raw JSON (useful for untyped/broker access).
    pub fn get_json(&self, id: &str) -> Result<Option<Value>> {
        let key = format!("{}{}", self.prefix, id);
        match self.store.get(&key)? {
            Some(am_doc) => Ok(Some(automerge_to_json(&am_doc))),
            None => Ok(None),
        }
    }

    /// Delete a document by ID.
    pub fn delete(&self, id: &str) -> Result<()> {
        let key = format!("{}{}", self.prefix, id);
        self.store.delete(&key)
    }

    /// Scan all documents in this collection, deserialized into `T`.
    ///
    /// Returns `(doc_id, T)` pairs. Documents that fail deserialization are skipped.
    pub fn scan(&self) -> Result<Vec<(String, T)>> {
        let docs = self.store.scan_prefix(&self.prefix)?;
        let mut results = Vec::new();
        for (key, am_doc) in docs {
            if let Some(id) = key.strip_prefix(&self.prefix) {
                let json = automerge_to_json(&am_doc);
                if let Ok(val) = serde_json::from_value(json) {
                    results.push((id.to_string(), val));
                }
            }
        }
        Ok(results)
    }

    /// Scan all documents in this collection as raw JSON.
    pub fn scan_json(&self) -> Result<Vec<(String, Value)>> {
        let docs = self.store.scan_prefix(&self.prefix)?;
        let mut results = Vec::new();
        for (key, am_doc) in docs {
            if let Some(id) = key.strip_prefix(&self.prefix) {
                results.push((id.to_string(), automerge_to_json(&am_doc)));
            }
        }
        Ok(results)
    }

    /// Count documents in this collection.
    pub fn count(&self) -> Result<usize> {
        Ok(self.store.scan_prefix(&self.prefix)?.len())
    }

    /// Start a query builder scoped to this collection.
    pub fn query(&self) -> Query {
        Query::new(self.store.clone(), &self.name)
    }

    /// Subscribe to local changes in this collection.
    ///
    /// Returns a stream of document IDs (without the collection prefix)
    /// that were modified locally. Useful for triggering sync.
    pub fn subscribe(&self) -> impl futures::Stream<Item = String> {
        let rx = self.store.subscribe_to_changes();
        let prefix = self.prefix.clone();
        tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |result| match result {
            Ok(key) => key.strip_prefix(&prefix).map(|id| id.to_string()),
            Err(_) => None,
        })
    }

    /// Subscribe to all changes (local + remote) in this collection.
    ///
    /// Returns a stream of document IDs (without the collection prefix)
    /// for all changes including those received via sync.
    pub fn subscribe_all(&self) -> impl futures::Stream<Item = String> {
        let rx = self.store.subscribe_to_observer_changes();
        let prefix = self.prefix.clone();
        tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |result| match result {
            Ok(key) => key.strip_prefix(&prefix).map(|id| id.to_string()),
            Err(_) => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    fn create_test_store() -> (Arc<AutomergeStore>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(AutomergeStore::open(temp_dir.path()).unwrap());
        (store, temp_dir)
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct SensorReading {
        device: String,
        temperature_c: f64,
    }

    #[test]
    fn test_typed_upsert_and_get() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");

        let reading = SensorReading {
            device: "sensor-42".into(),
            temperature_c: 23.5,
        };
        sensors.upsert("r001", &reading).unwrap();

        let result = sensors.get("r001").unwrap().unwrap();
        assert_eq!(result.device, "sensor-42");
        assert_eq!(result.temperature_c, 23.5);
    }

    #[test]
    fn test_typed_get_missing() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");
        assert!(sensors.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_typed_scan() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");

        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "a".into(),
                    temperature_c: 20.0,
                },
            )
            .unwrap();
        sensors
            .upsert(
                "r002",
                &SensorReading {
                    device: "b".into(),
                    temperature_c: 30.0,
                },
            )
            .unwrap();

        let results = sensors.scan().unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_typed_delete() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");

        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "a".into(),
                    temperature_c: 20.0,
                },
            )
            .unwrap();
        assert!(sensors.get("r001").unwrap().is_some());

        sensors.delete("r001").unwrap();
        assert!(sensors.get("r001").unwrap().is_none());
    }

    #[test]
    fn test_typed_count() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");

        assert_eq!(sensors.count().unwrap(), 0);

        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "a".into(),
                    temperature_c: 20.0,
                },
            )
            .unwrap();
        sensors
            .upsert(
                "r002",
                &SensorReading {
                    device: "b".into(),
                    temperature_c: 30.0,
                },
            )
            .unwrap();

        assert_eq!(sensors.count().unwrap(), 2);
    }

    #[test]
    fn test_typed_get_json() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");

        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "x".into(),
                    temperature_c: 25.0,
                },
            )
            .unwrap();

        let json = sensors.get_json("r001").unwrap().unwrap();
        assert_eq!(json["device"], "x");
        assert_eq!(json["temperature_c"], 25.0);
    }

    #[test]
    fn test_typed_scan_json() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");

        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "a".into(),
                    temperature_c: 20.0,
                },
            )
            .unwrap();

        let results = sensors.scan_json().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "r001");
        assert_eq!(results[0].1["device"], "a");
    }

    #[test]
    fn test_typed_query() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");

        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "a".into(),
                    temperature_c: 20.0,
                },
            )
            .unwrap();
        sensors
            .upsert(
                "r002",
                &SensorReading {
                    device: "b".into(),
                    temperature_c: 30.0,
                },
            )
            .unwrap();

        let results = sensors
            .query()
            .where_gt("temperature_c", crate::storage::Value::Float(25.0))
            .execute_json()
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1["device"], "b");
    }

    #[test]
    fn test_typed_upsert_preserves_crdt() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");

        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "a".into(),
                    temperature_c: 20.0,
                },
            )
            .unwrap();

        // Update same doc — should fork, not create new
        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "a".into(),
                    temperature_c: 25.0,
                },
            )
            .unwrap();

        let result = sensors.get("r001").unwrap().unwrap();
        assert_eq!(result.temperature_c, 25.0);
    }

    #[test]
    fn test_collection_isolation() {
        let (store, _temp) = create_test_store();
        let sensors = store.typed_collection::<SensorReading>("sensors");
        let other = store.typed_collection::<SensorReading>("other");

        sensors
            .upsert(
                "r001",
                &SensorReading {
                    device: "a".into(),
                    temperature_c: 20.0,
                },
            )
            .unwrap();

        assert!(other.get("r001").unwrap().is_none());
        assert_eq!(other.count().unwrap(), 0);
        assert_eq!(sensors.count().unwrap(), 1);
    }
}
