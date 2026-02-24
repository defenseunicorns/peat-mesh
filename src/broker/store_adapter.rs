//! Adapter that exposes AutomergeStore documents via broker endpoints.
//!
//! Bridges the storage layer with the broker's document API by converting
//! Automerge documents to JSON and injecting `_id` fields.

use std::sync::Arc;

use serde_json::Value;

use crate::storage::json_convert::automerge_to_json;
use crate::storage::AutomergeStore;

/// Adapts an `AutomergeStore` for use by the broker document endpoints.
pub struct StoreBrokerAdapter {
    store: Arc<AutomergeStore>,
}

impl StoreBrokerAdapter {
    /// Create a new adapter wrapping the given store.
    pub fn new(store: Arc<AutomergeStore>) -> Self {
        Self { store }
    }

    /// List all documents in a collection as JSON, with `_id` injected.
    ///
    /// Returns `None` if the collection has no documents.
    pub fn list_documents(&self, collection: &str) -> Option<Vec<Value>> {
        let prefix = format!("{}:", collection);
        let docs = self.store.scan_prefix(&prefix).ok()?;
        if docs.is_empty() {
            return None;
        }
        let results: Vec<Value> = docs
            .into_iter()
            .filter_map(|(key, am_doc)| {
                let id = key.strip_prefix(&prefix)?;
                let mut json = automerge_to_json(&am_doc);
                if let Value::Object(ref mut map) = json {
                    map.insert("_id".to_string(), Value::String(id.to_string()));
                }
                Some(json)
            })
            .collect();
        Some(results)
    }

    /// Get a single document by collection and ID as JSON, with `_id` injected.
    pub fn get_document(&self, collection: &str, id: &str) -> Option<Value> {
        let key = format!("{}:{}", collection, id);
        let am_doc = self.store.get(&key).ok()??;
        let mut json = automerge_to_json(&am_doc);
        if let Value::Object(ref mut map) = json {
            map.insert("_id".to_string(), Value::String(id.to_string()));
        }
        Some(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::json_convert::json_to_automerge;
    use tempfile::TempDir;

    fn create_test_store() -> (Arc<AutomergeStore>, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let store = Arc::new(AutomergeStore::open(temp_dir.path()).unwrap());
        (store, temp_dir)
    }

    #[test]
    fn test_list_documents_empty() {
        let (store, _temp) = create_test_store();
        let adapter = StoreBrokerAdapter::new(store);
        assert!(adapter.list_documents("test").is_none());
    }

    #[test]
    fn test_list_documents() {
        let (store, _temp) = create_test_store();

        let json = serde_json::json!({"name": "doc1", "value": 42});
        let am_doc = json_to_automerge(&json, None).unwrap();
        store.put("test:doc1", &am_doc).unwrap();

        let adapter = StoreBrokerAdapter::new(store);
        let docs = adapter.list_documents("test").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["_id"], "doc1");
        assert_eq!(docs[0]["name"], "doc1");
        assert_eq!(docs[0]["value"], 42);
    }

    #[test]
    fn test_get_document_missing() {
        let (store, _temp) = create_test_store();
        let adapter = StoreBrokerAdapter::new(store);
        assert!(adapter.get_document("test", "missing").is_none());
    }

    #[test]
    fn test_get_document() {
        let (store, _temp) = create_test_store();

        let json = serde_json::json!({"name": "doc1"});
        let am_doc = json_to_automerge(&json, None).unwrap();
        store.put("test:doc1", &am_doc).unwrap();

        let adapter = StoreBrokerAdapter::new(store);
        let doc = adapter.get_document("test", "doc1").unwrap();
        assert_eq!(doc["_id"], "doc1");
        assert_eq!(doc["name"], "doc1");
    }

    #[test]
    fn test_list_documents_multiple_collections() {
        let (store, _temp) = create_test_store();

        let json_a = serde_json::json!({"name": "a"});
        let json_b = serde_json::json!({"name": "b"});
        store
            .put("col1:doc1", &json_to_automerge(&json_a, None).unwrap())
            .unwrap();
        store
            .put("col2:doc1", &json_to_automerge(&json_b, None).unwrap())
            .unwrap();

        let adapter = StoreBrokerAdapter::new(store);

        let col1_docs = adapter.list_documents("col1").unwrap();
        assert_eq!(col1_docs.len(), 1);
        assert_eq!(col1_docs[0]["name"], "a");

        let col2_docs = adapter.list_documents("col2").unwrap();
        assert_eq!(col2_docs.len(), 1);
        assert_eq!(col2_docs[0]["name"], "b");
    }
}
