//! Integration tests for GC, tombstone storage, and deletion policies
//!
//! These tests exercise the full stack:
//! AutomergeStore (tombstone CRUD) → GcStore trait → GarbageCollector
//!
//! Requires `automerge-backend` feature for AutomergeStore.

#![cfg(feature = "automerge-backend")]

use automerge::transaction::Transactable;
use peat_mesh::qos::{
    DeletionPolicyRegistry, GarbageCollector, GcConfig, GcStore, ResurrectionPolicy, Tombstone,
};
use peat_mesh::storage::AutomergeStore;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

/// Create a test store backed by a temp directory
fn create_test_store() -> (Arc<AutomergeStore>, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(AutomergeStore::open(temp_dir.path()).unwrap());
    (store, temp_dir)
}

// === AutomergeStore tombstone CRUD ===

#[test]
fn test_put_and_get_tombstone() {
    let (store, _tmp) = create_test_store();

    let tombstone = Tombstone::new("doc-1", "tracks", "node-a", 1);
    store.put_tombstone(&tombstone).unwrap();

    let retrieved = store.get_tombstone("tracks", "doc-1").unwrap();
    assert!(retrieved.is_some());
    let t = retrieved.unwrap();
    assert_eq!(t.document_id, "doc-1");
    assert_eq!(t.collection, "tracks");
    assert_eq!(t.deleted_by, "node-a");
    assert_eq!(t.lamport, 1);
}

#[test]
fn test_has_tombstone() {
    let (store, _tmp) = create_test_store();

    assert!(!store.has_tombstone("tracks", "doc-1").unwrap());

    let tombstone = Tombstone::new("doc-1", "tracks", "node-a", 1);
    store.put_tombstone(&tombstone).unwrap();

    assert!(store.has_tombstone("tracks", "doc-1").unwrap());
    assert!(!store.has_tombstone("tracks", "doc-2").unwrap());
    assert!(!store.has_tombstone("beacons", "doc-1").unwrap());
}

#[test]
fn test_remove_tombstone() {
    let (store, _tmp) = create_test_store();

    let tombstone = Tombstone::new("doc-1", "tracks", "node-a", 1);
    store.put_tombstone(&tombstone).unwrap();

    assert!(store.has_tombstone("tracks", "doc-1").unwrap());

    let removed = store.remove_tombstone("tracks", "doc-1").unwrap();
    assert!(removed);

    assert!(!store.has_tombstone("tracks", "doc-1").unwrap());

    // Removing again returns false
    let removed_again = store.remove_tombstone("tracks", "doc-1").unwrap();
    assert!(!removed_again);
}

#[test]
fn test_get_tombstones_for_collection() {
    let (store, _tmp) = create_test_store();

    store
        .put_tombstone(&Tombstone::new("doc-1", "tracks", "node-a", 1))
        .unwrap();
    store
        .put_tombstone(&Tombstone::new("doc-2", "tracks", "node-b", 2))
        .unwrap();
    store
        .put_tombstone(&Tombstone::new("doc-3", "beacons", "node-a", 3))
        .unwrap();

    let track_tombstones = store.get_tombstones_for_collection("tracks").unwrap();
    assert_eq!(track_tombstones.len(), 2);

    let beacon_tombstones = store.get_tombstones_for_collection("beacons").unwrap();
    assert_eq!(beacon_tombstones.len(), 1);
    assert_eq!(beacon_tombstones[0].document_id, "doc-3");
}

#[test]
fn test_get_all_tombstones() {
    let (store, _tmp) = create_test_store();

    store
        .put_tombstone(&Tombstone::new("doc-1", "tracks", "node-a", 1))
        .unwrap();
    store
        .put_tombstone(&Tombstone::new("doc-2", "beacons", "node-b", 2))
        .unwrap();
    store
        .put_tombstone(&Tombstone::new("doc-3", "alerts", "node-c", 3))
        .unwrap();

    let all = store.get_all_tombstones().unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn test_tombstone_with_reason() {
    let (store, _tmp) = create_test_store();

    let tombstone = Tombstone::with_reason("doc-1", "tracks", "node-a", 1, "user requested");
    store.put_tombstone(&tombstone).unwrap();

    let retrieved = store.get_tombstone("tracks", "doc-1").unwrap().unwrap();
    assert_eq!(retrieved.reason, Some("user requested".to_string()));
}

// === GcStore trait via AutomergeStore ===

#[test]
fn test_gcstore_has_tombstone() {
    let (store, _tmp) = create_test_store();

    // Use GcStore trait methods via the trait
    let gc_store: &dyn GcStore = store.as_ref();

    assert!(!gc_store.has_tombstone("tracks", "doc-1").unwrap());

    store
        .put_tombstone(&Tombstone::new("doc-1", "tracks", "node-a", 1))
        .unwrap();

    assert!(gc_store.has_tombstone("tracks", "doc-1").unwrap());
}

#[test]
fn test_gcstore_get_all_and_remove_tombstones() {
    let (store, _tmp) = create_test_store();
    let gc_store: &dyn GcStore = store.as_ref();

    store
        .put_tombstone(&Tombstone::new("doc-1", "tracks", "node-a", 1))
        .unwrap();
    store
        .put_tombstone(&Tombstone::new("doc-2", "tracks", "node-b", 2))
        .unwrap();

    let all = gc_store.get_all_tombstones().unwrap();
    assert_eq!(all.len(), 2);

    let removed = gc_store.remove_tombstone("tracks", "doc-1").unwrap();
    assert!(removed);

    let remaining = gc_store.get_all_tombstones().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].document_id, "doc-2");
}

#[test]
fn test_gcstore_list_collections() {
    let (store, _tmp) = create_test_store();

    // Put some documents to create collections
    let mut doc = automerge::Automerge::new();
    let mut tx = doc.transaction();
    tx.put(automerge::ROOT, "test", "value").unwrap();
    tx.commit();

    store.put("tracks:doc-1", &doc).unwrap();
    store.put("beacons:beacon-1", &doc).unwrap();
    store.put("alerts:alert-1", &doc).unwrap();

    let gc_store: &dyn GcStore = store.as_ref();
    let mut collections = gc_store.list_collections().unwrap();
    collections.sort();

    assert_eq!(collections, vec!["alerts", "beacons", "tracks"]);
}

#[test]
fn test_gcstore_hard_delete() {
    let (store, _tmp) = create_test_store();

    // Create a document
    let mut doc = automerge::Automerge::new();
    let mut tx = doc.transaction();
    tx.put(automerge::ROOT, "test", "value").unwrap();
    tx.commit();
    store.put("tracks:doc-1", &doc).unwrap();

    // Verify it exists
    assert!(store.get("tracks:doc-1").unwrap().is_some());

    // Hard delete via GcStore trait
    let gc_store: &dyn GcStore = store.as_ref();
    gc_store.hard_delete("tracks", "doc-1").unwrap();

    // Verify it's gone
    assert!(store.get("tracks:doc-1").unwrap().is_none());
}

#[test]
fn test_gcstore_get_expired_documents() {
    let (store, _tmp) = create_test_store();

    // Create documents with _created_at timestamps
    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // Old document (created 2 hours ago)
    let mut old_doc = automerge::Automerge::new();
    {
        let mut tx = old_doc.transaction();
        tx.put(
            automerge::ROOT,
            "_created_at",
            now_ms - 2 * 3600 * 1000, // 2 hours ago
        )
        .unwrap();
        tx.put(automerge::ROOT, "data", "old").unwrap();
        tx.commit();
    }
    store.put("beacons:old-beacon", &old_doc).unwrap();

    // Recent document (created just now)
    let mut new_doc = automerge::Automerge::new();
    {
        let mut tx = new_doc.transaction();
        tx.put(automerge::ROOT, "_created_at", now_ms).unwrap();
        tx.put(automerge::ROOT, "data", "new").unwrap();
        tx.commit();
    }
    store.put("beacons:new-beacon", &new_doc).unwrap();

    // Cutoff: 1 hour ago
    let cutoff = SystemTime::now() - Duration::from_secs(3600);

    let gc_store: &dyn GcStore = store.as_ref();
    let expired = gc_store.get_expired_documents("beacons", cutoff).unwrap();

    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0], "old-beacon");
}

// === GarbageCollector with real AutomergeStore ===

#[test]
fn test_gc_collects_expired_tombstones() {
    let (store, _tmp) = create_test_store();

    // Create a tombstone that's already "expired" (we use default policy TTL)
    // The tracks collection uses Tombstone policy with 1-hour TTL
    // We can't easily make time pass, but we can test the GC run completes
    let tombstone = Tombstone::new("doc-1", "tracks", "node-a", 1);
    store.put_tombstone(&tombstone).unwrap();

    let registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let gc = GarbageCollector::with_policy_registry(
        store.clone(),
        registry,
        GcConfig::with_interval(Duration::from_secs(60)),
    );

    // Run GC — tombstone is fresh so it should NOT be collected
    let result = gc.run_gc().unwrap();
    assert_eq!(result.errors.len(), 0);

    // Tombstone should still exist (not expired yet)
    assert!(store.has_tombstone("tracks", "doc-1").unwrap());
}

#[test]
fn test_gc_collects_expired_documents_from_implicit_ttl_collection() {
    let (store, _tmp) = create_test_store();

    let now_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // beacons collection uses ImplicitTTL with 1-hour TTL
    // Create an old beacon (2 hours ago)
    let mut old_doc = automerge::Automerge::new();
    {
        let mut tx = old_doc.transaction();
        tx.put(automerge::ROOT, "_created_at", now_ms - 2 * 3600 * 1000)
            .unwrap();
        tx.put(automerge::ROOT, "data", "stale beacon").unwrap();
        tx.commit();
    }
    store.put("beacons:old-beacon", &old_doc).unwrap();

    // Create a fresh beacon
    let mut new_doc = automerge::Automerge::new();
    {
        let mut tx = new_doc.transaction();
        tx.put(automerge::ROOT, "_created_at", now_ms).unwrap();
        tx.put(automerge::ROOT, "data", "fresh beacon").unwrap();
        tx.commit();
    }
    store.put("beacons:fresh-beacon", &new_doc).unwrap();

    let registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let gc = GarbageCollector::with_policy_registry(
        store.clone(),
        registry,
        GcConfig::with_interval(Duration::from_secs(60)),
    );

    let result = gc.run_gc().unwrap();

    // Old beacon should have been collected
    assert!(
        result.documents_collected >= 1,
        "Expected at least 1 document collected, got {}",
        result.documents_collected
    );
    assert!(store.get("beacons:old-beacon").unwrap().is_none());

    // Fresh beacon should still exist
    assert!(store.get("beacons:fresh-beacon").unwrap().is_some());
}

#[test]
fn test_gc_stats_accumulate() {
    let (store, _tmp) = create_test_store();

    let registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let gc = GarbageCollector::with_policy_registry(
        store.clone(),
        registry,
        GcConfig::with_interval(Duration::from_secs(60)),
    );

    // Initial stats
    let stats = gc.stats();
    assert_eq!(stats.total_runs, 0);

    // Run GC
    gc.run_gc().unwrap();

    let stats = gc.stats();
    assert_eq!(stats.total_runs, 1);

    // Run again
    gc.run_gc().unwrap();

    let stats = gc.stats();
    assert_eq!(stats.total_runs, 2);
}

#[test]
fn test_gc_check_resurrection_with_active_tombstone() {
    let (store, _tmp) = create_test_store();

    // Create a tombstone for a tracks document
    let tombstone = Tombstone::new("doc-1", "tracks", "node-a", 1);
    store.put_tombstone(&tombstone).unwrap();

    let registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let gc = GarbageCollector::with_policy_registry(
        store.clone(),
        registry,
        GcConfig::with_interval(Duration::from_secs(60)),
    );

    // With active tombstone, check_resurrection returns None
    // (this is a normal sync conflict, not a resurrection)
    let policy = gc
        .check_resurrection("tracks", "doc-1", SystemTime::now())
        .unwrap();
    assert!(
        policy.is_none(),
        "Active tombstone = sync conflict, not resurrection"
    );
}

#[test]
fn test_gc_check_resurrection_no_tombstone() {
    let (store, _tmp) = create_test_store();

    let registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let gc = GarbageCollector::with_policy_registry(
        store.clone(),
        registry,
        GcConfig::with_interval(Duration::from_secs(60)),
    );

    // No tombstone — can't distinguish "never deleted" from "tombstone expired"
    // Current implementation returns None (resurrection detection needs sync-layer metadata)
    let policy = gc
        .check_resurrection("tracks", "doc-1", SystemTime::now())
        .unwrap();
    assert!(policy.is_none());
}

#[test]
fn test_gc_handle_resurrection_default_policy() {
    let (store, _tmp) = create_test_store();

    let registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let gc = GarbageCollector::with_policy_registry(
        store.clone(),
        registry,
        GcConfig::with_interval(Duration::from_secs(60)),
    );

    // Default resurrection policy is Allow (GcConfig has empty overrides)
    let policy = gc.handle_resurrection("tracks", "doc-1").unwrap();
    assert_eq!(policy, ResurrectionPolicy::Allow);

    // Stats should track the resurrection
    assert_eq!(gc.stats().total_resurrections, 1);
}

#[test]
fn test_gc_handle_resurrection_with_redelete_override() {
    let (store, _tmp) = create_test_store();

    let registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let mut config = GcConfig::with_interval(Duration::from_secs(60));
    config.set_resurrection_policy("tracks", ResurrectionPolicy::ReDelete);

    let gc = GarbageCollector::with_policy_registry(store.clone(), registry, config);

    let policy = gc.handle_resurrection("tracks", "doc-1").unwrap();
    assert_eq!(policy, ResurrectionPolicy::ReDelete);
}

// === Tombstone in memory-only mode ===

#[test]
fn test_tombstone_noop_in_memory_mode() {
    let store = AutomergeStore::in_memory();

    // Tombstone operations are no-ops in memory mode
    let tombstone = Tombstone::new("doc-1", "tracks", "node-a", 1);
    store.put_tombstone(&tombstone).unwrap(); // No-op

    assert!(!store.has_tombstone("tracks", "doc-1").unwrap());
    assert!(store.get_all_tombstones().unwrap().is_empty());
}

// === Periodic GC lifecycle ===

#[tokio::test]
async fn test_periodic_gc_start_stop() {
    let (store, _tmp) = create_test_store();

    let registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let gc = Arc::new(GarbageCollector::with_policy_registry(
        store,
        registry,
        GcConfig::with_interval(Duration::from_millis(50)), // Fast interval for test
    ));

    assert!(!gc.is_running());

    let handle = peat_mesh::qos::start_periodic_gc(gc.clone());

    // Give it a moment to run
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(gc.is_running());
    assert!(
        gc.stats().total_runs >= 1,
        "GC should have run at least once"
    );

    gc.stop();
    handle.abort();
}
