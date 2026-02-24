//! Automerge document operations — create, store, query, and subscribe.
//!
//! Run with: `cargo run --example document_sync --features automerge-backend`

use automerge::{transaction::Transactable, Automerge, ReadDoc, ROOT};
use eche_mesh::storage::AutomergeStore;
use std::sync::Arc;

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // 1. Open a disk-backed store (using a tempdir for the example).
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let store = Arc::new(
            AutomergeStore::open(tmp.path().join("automerge")).expect("failed to open store"),
        );
        println!("Store opened at {:?}", tmp.path());

        // 2. Subscribe to change notifications.
        let mut changes = store.subscribe_to_changes();
        let change_task = tokio::spawn(async move {
            while let Ok(key) = changes.recv().await {
                println!("  [change] document updated: {}", key);
            }
        });

        // 3. Create and store a document.
        let mut doc = Automerge::new();
        doc.transact::<_, _, automerge::AutomergeError>(|tx| {
            tx.put(ROOT, "type", "sensor-reading")?;
            tx.put(ROOT, "device", "sensor-42")?;
            tx.put(ROOT, "temperature_c", 23.5)?;
            tx.put(ROOT, "timestamp", "2026-02-22T10:00:00Z")?;
            Ok(())
        })
        .expect("transaction failed");

        store
            .put("sensors:reading-001", &doc)
            .expect("failed to store document");
        println!("Stored sensors:reading-001");

        // 4. Retrieve and read the document back.
        let loaded = store
            .get("sensors:reading-001")
            .expect("get failed")
            .expect("document not found");

        let device = loaded
            .get(ROOT, "device")
            .unwrap()
            .map(|(val, _)| val.to_string());
        let temp = loaded
            .get(ROOT, "temperature_c")
            .unwrap()
            .map(|(val, _)| val.to_string());
        println!("Retrieved: device={:?}, temperature={:?}", device, temp);

        // 5. Store a second document in the same collection prefix.
        let mut doc2 = Automerge::new();
        doc2.transact::<_, _, automerge::AutomergeError>(|tx| {
            tx.put(ROOT, "type", "sensor-reading")?;
            tx.put(ROOT, "device", "sensor-99")?;
            tx.put(ROOT, "temperature_c", 18.0)?;
            Ok(())
        })
        .expect("transaction failed");

        store
            .put("sensors:reading-002", &doc2)
            .expect("failed to store document");

        // 6. Use scan_prefix for collection-style access.
        let sensors = store.scan_prefix("sensors:").expect("scan_prefix failed");
        println!(
            "\nAll documents in 'sensors:' collection ({} total):",
            sensors.len()
        );
        for (key, doc) in &sensors {
            let device = doc
                .get(ROOT, "device")
                .unwrap()
                .map(|(v, _)| v.to_string())
                .unwrap_or_default();
            println!("  {} -> device={}", key, device);
        }

        // 7. List total document count.
        println!("\nTotal documents in store: {}", store.count());

        // Let change notifications flush.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        change_task.abort();

        println!("Done.");
    });
}
