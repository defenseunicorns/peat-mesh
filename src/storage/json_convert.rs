//! Bidirectional Automerge <-> JSON conversion
//!
//! Provides utilities for converting between Automerge documents and
//! `serde_json::Value`, enabling typed collection access via serde.

use anyhow::{Context, Result};
use automerge::{transaction::Transactable, Automerge, ObjType, ReadDoc, ScalarValue};
use serde_json::Value;

/// Convert an Automerge document's root map to a JSON value.
///
/// Recursively walks the root map, converting scalars, nested maps,
/// and lists into their JSON equivalents.
pub fn automerge_to_json(doc: &Automerge) -> Value {
    map_to_json(doc, &automerge::ROOT)
}

/// Convert an Automerge map object to a JSON object.
fn map_to_json(doc: &Automerge, obj: &automerge::ObjId) -> Value {
    let mut map = serde_json::Map::new();
    for key in doc.keys(obj) {
        if let Ok(Some((value, child_id))) = doc.get(obj, &*key) {
            map.insert(key, am_value_to_json(doc, &value, &child_id));
        }
    }
    Value::Object(map)
}

/// Convert an Automerge list object to a JSON array.
fn list_to_json(doc: &Automerge, obj: &automerge::ObjId) -> Value {
    let len = doc.length(obj);
    let mut arr = Vec::with_capacity(len);
    for i in 0..len {
        if let Ok(Some((value, child_id))) = doc.get(obj, i) {
            arr.push(am_value_to_json(doc, &value, &child_id));
        }
    }
    Value::Array(arr)
}

/// Convert a single Automerge value to JSON.
fn am_value_to_json(
    doc: &Automerge,
    value: &automerge::Value,
    child_id: &automerge::ObjId,
) -> Value {
    match value {
        automerge::Value::Scalar(scalar) => scalar_to_json(scalar.as_ref()),
        automerge::Value::Object(ObjType::Map) => map_to_json(doc, child_id),
        automerge::Value::Object(ObjType::List) => list_to_json(doc, child_id),
        automerge::Value::Object(_) => Value::Null,
    }
}

/// Convert an Automerge scalar value to JSON.
fn scalar_to_json(scalar: &ScalarValue) -> Value {
    match scalar {
        ScalarValue::Null => Value::Null,
        ScalarValue::Boolean(b) => Value::Bool(*b),
        ScalarValue::Int(i) => Value::Number((*i).into()),
        ScalarValue::Uint(u) => Value::Number((*u).into()),
        ScalarValue::F64(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ScalarValue::Str(s) => Value::String(s.to_string()),
        ScalarValue::Timestamp(t) => Value::Number((*t).into()),
        ScalarValue::Bytes(_) | ScalarValue::Counter(_) | ScalarValue::Unknown { .. } => {
            Value::Null
        }
    }
}

/// Convert a JSON value into an Automerge document.
///
/// If `existing` is provided, the document is forked (preserving CRDT history)
/// and updated with the new JSON fields. Keys present in the existing document
/// but absent from `value` are deleted.
///
/// The root `value` must be a JSON object.
pub fn json_to_automerge(value: &Value, existing: Option<&Automerge>) -> Result<Automerge> {
    let map = value
        .as_object()
        .context("Root value must be a JSON object")?;

    let mut doc = match existing {
        Some(existing) => existing.fork(),
        None => Automerge::new(),
    };

    // Collect existing root keys before the transaction
    let existing_keys: Vec<String> = doc.keys(&automerge::ROOT).collect();

    doc.transact::<_, _, automerge::AutomergeError>(|tx| {
        let json_keys: std::collections::HashSet<&str> = map.keys().map(|k| k.as_str()).collect();

        for (key, val) in map {
            write_json_value(tx, &automerge::ROOT, key, val)?;
        }

        // Delete stale root keys
        for key in &existing_keys {
            if !json_keys.contains(key.as_str()) {
                tx.delete(&automerge::ROOT, key.as_str())?;
            }
        }

        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("Automerge transaction failed: {:?}", e))?;

    Ok(doc)
}

/// Write a JSON value into an Automerge object at the given key.
fn write_json_value(
    tx: &mut impl Transactable,
    obj: &automerge::ObjId,
    key: &str,
    value: &Value,
) -> Result<(), automerge::AutomergeError> {
    match value {
        Value::Null => {
            tx.put(obj, key, ScalarValue::Null)?;
        }
        Value::Bool(b) => {
            tx.put(obj, key, *b)?;
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                tx.put(obj, key, i)?;
            } else if let Some(f) = n.as_f64() {
                tx.put(obj, key, f)?;
            }
        }
        Value::String(s) => {
            tx.put(obj, key, s.as_str())?;
        }
        Value::Array(arr) => {
            let list_id = tx.put_object(obj, key, ObjType::List)?;
            for (i, item) in arr.iter().enumerate() {
                write_json_list_item(tx, &list_id, i, item)?;
            }
        }
        Value::Object(map) => {
            let map_id = tx.put_object(obj, key, ObjType::Map)?;
            for (k, v) in map {
                write_json_value(tx, &map_id, k, v)?;
            }
        }
    }
    Ok(())
}

/// Write a JSON value as a list item at the given index.
fn write_json_list_item(
    tx: &mut impl Transactable,
    list: &automerge::ObjId,
    index: usize,
    value: &Value,
) -> Result<(), automerge::AutomergeError> {
    match value {
        Value::Null => {
            tx.insert(list, index, ScalarValue::Null)?;
        }
        Value::Bool(b) => {
            tx.insert(list, index, *b)?;
        }
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                tx.insert(list, index, i)?;
            } else if let Some(f) = n.as_f64() {
                tx.insert(list, index, f)?;
            }
        }
        Value::String(s) => {
            tx.insert(list, index, s.as_str())?;
        }
        Value::Array(arr) => {
            let nested_list = tx.insert_object(list, index, ObjType::List)?;
            for (i, item) in arr.iter().enumerate() {
                write_json_list_item(tx, &nested_list, i, item)?;
            }
        }
        Value::Object(map) => {
            let nested_map = tx.insert_object(list, index, ObjType::Map)?;
            for (k, v) in map {
                write_json_value(tx, &nested_map, k, v)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use automerge::transaction::Transactable;

    #[test]
    fn test_automerge_to_json_simple() {
        let mut doc = Automerge::new();
        doc.transact::<_, _, automerge::AutomergeError>(|tx| {
            tx.put(automerge::ROOT, "name", "test")?;
            tx.put(automerge::ROOT, "count", 42i64)?;
            tx.put(automerge::ROOT, "active", true)?;
            Ok(())
        })
        .unwrap();

        let json = automerge_to_json(&doc);
        assert_eq!(json["name"], "test");
        assert_eq!(json["count"], 42);
        assert_eq!(json["active"], true);
    }

    #[test]
    fn test_automerge_to_json_nested_map() {
        let mut doc = Automerge::new();
        doc.transact::<_, _, automerge::AutomergeError>(|tx| {
            let pos = tx.put_object(automerge::ROOT, "position", ObjType::Map)?;
            tx.put(&pos, "lat", 37.7749)?;
            tx.put(&pos, "lon", -122.4194)?;
            Ok(())
        })
        .unwrap();

        let json = automerge_to_json(&doc);
        assert_eq!(json["position"]["lat"], 37.7749);
        assert_eq!(json["position"]["lon"], -122.4194);
    }

    #[test]
    fn test_automerge_to_json_list() {
        let mut doc = Automerge::new();
        doc.transact::<_, _, automerge::AutomergeError>(|tx| {
            let tags = tx.put_object(automerge::ROOT, "tags", ObjType::List)?;
            tx.insert(&tags, 0, "alpha")?;
            tx.insert(&tags, 1, "beta")?;
            Ok(())
        })
        .unwrap();

        let json = automerge_to_json(&doc);
        assert_eq!(json["tags"][0], "alpha");
        assert_eq!(json["tags"][1], "beta");
    }

    #[test]
    fn test_json_to_automerge_roundtrip() {
        let json = serde_json::json!({
            "name": "test",
            "count": 42,
            "active": true,
            "tags": ["a", "b"]
        });

        let doc = json_to_automerge(&json, None).unwrap();
        let result = automerge_to_json(&doc);

        assert_eq!(result["name"], "test");
        assert_eq!(result["count"], 42);
        assert_eq!(result["active"], true);
        assert_eq!(result["tags"][0], "a");
        assert_eq!(result["tags"][1], "b");
    }

    #[test]
    fn test_json_to_automerge_nested_roundtrip() {
        let json = serde_json::json!({
            "position": {"lat": 37.7, "lon": -122.4},
            "name": "hq"
        });

        let doc = json_to_automerge(&json, None).unwrap();
        let result = automerge_to_json(&doc);

        assert_eq!(result["position"]["lat"], 37.7);
        assert_eq!(result["position"]["lon"], -122.4);
        assert_eq!(result["name"], "hq");
    }

    #[test]
    fn test_json_to_automerge_preserves_crdt() {
        let json1 = serde_json::json!({"name": "v1", "count": 1});
        let doc1 = json_to_automerge(&json1, None).unwrap();

        let json2 = serde_json::json!({"name": "v2", "count": 2});
        let doc2 = json_to_automerge(&json2, Some(&doc1)).unwrap();

        let result = automerge_to_json(&doc2);
        assert_eq!(result["name"], "v2");
        assert_eq!(result["count"], 2);
    }

    #[test]
    fn test_json_to_automerge_deletes_stale_keys() {
        let json1 = serde_json::json!({"name": "test", "old_field": "will be deleted"});
        let doc1 = json_to_automerge(&json1, None).unwrap();

        let json2 = serde_json::json!({"name": "test"});
        let doc2 = json_to_automerge(&json2, Some(&doc1)).unwrap();

        let result = automerge_to_json(&doc2);
        assert_eq!(result["name"], "test");
        assert!(result.get("old_field").is_none());
    }

    #[test]
    fn test_empty_document() {
        let doc = Automerge::new();
        let json = automerge_to_json(&doc);
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn test_json_to_automerge_rejects_non_object() {
        let json = serde_json::json!("just a string");
        assert!(json_to_automerge(&json, None).is_err());
    }
}
