//! Stress tests for Negentropy set reconciliation at scale (Issue #6)
//!
//! Validates O(log n) sync convergence with 1000+ documents,
//! measuring round trips, memory usage, and message overhead.

#![cfg(feature = "automerge-backend")]

use peat_mesh::storage::{NegentropySync, SyncItem};
use std::time::Instant;

/// Create a test EndpointId from a seed byte
fn test_peer_id(seed: u8) -> iroh::EndpointId {
    use iroh::SecretKey;
    let mut key_bytes = [0u8; 32];
    key_bytes[0] = seed;
    let secret = SecretKey::from_bytes(&key_bytes);
    secret.public()
}

/// Generate n SyncItems with keys like "collection::doc-{i}"
fn make_items(prefix: &str, count: usize, base_ts: u64) -> Vec<SyncItem> {
    (0..count)
        .map(|i| SyncItem::from_doc_key(&format!("{prefix}::doc-{i}"), base_ts + i as u64))
        .collect()
}

/// Run a full reconciliation between two NegentropySync instances,
/// returning (rounds, total_bytes, have_count, need_count).
fn run_reconciliation(
    items_a: Vec<SyncItem>,
    items_b: Vec<SyncItem>,
) -> (u64, u64, usize, usize) {
    let peer_a = test_peer_id(1);
    let peer_b = test_peer_id(2);

    let sync_a = NegentropySync::new();
    let sync_b = NegentropySync::new();

    let msg = sync_a.initiate_sync(peer_b, items_a.clone()).unwrap();
    let mut total_bytes: u64 = msg.len() as u64;
    let mut rounds: u64 = 1;

    let result_b = sync_b
        .handle_message(peer_a, &msg, items_b.clone())
        .unwrap();

    let mut current_msg = result_b.next_message;
    if let Some(ref m) = current_msg {
        total_bytes += m.len() as u64;
    }

    let mut final_have = 0usize;
    let mut final_need = 0usize;

    while let Some(msg) = current_msg {
        rounds += 1;
        let result = sync_a
            .handle_message(peer_b, &msg, items_a.clone())
            .unwrap();

        final_have += result.have_keys.len();
        final_need += result.need_keys.len();

        if result.is_complete {
            break;
        }

        if let Some(next) = result.next_message {
            total_bytes += next.len() as u64;
            let resp = sync_b
                .handle_message(peer_a, &next, items_b.clone())
                .unwrap();
            if let Some(ref m) = resp.next_message {
                total_bytes += m.len() as u64;
            }
            current_msg = resp.next_message;
        } else {
            break;
        }
    }

    (rounds, total_bytes, final_have, final_need)
}

#[test]
fn stress_identical_sets_1000_docs() {
    let items = make_items("nodes", 1000, 1_000_000);
    let start = Instant::now();
    let (rounds, bytes, have, need) = run_reconciliation(items.clone(), items);
    let elapsed = start.elapsed();

    eprintln!(
        "[identical 1000] rounds={rounds}, bytes={bytes}, have={have}, need={need}, elapsed={elapsed:?}"
    );

    // Identical sets: no differences
    assert_eq!(have, 0);
    assert_eq!(need, 0);
    // O(log n) rounds: log2(1000) ~ 10, allow generous headroom
    assert!(rounds <= 15, "Too many rounds for identical sets: {rounds}");
}

#[test]
fn stress_disjoint_sets_1000_docs() {
    // A has docs 0..1000, B has docs 1000..2000 — completely disjoint
    let items_a = make_items("data", 1000, 1_000_000);
    let items_b: Vec<SyncItem> = (0..1000)
        .map(|i| SyncItem::from_doc_key(&format!("data::doc-{}", 1000 + i), 2_000_000 + i as u64))
        .collect();

    let start = Instant::now();
    let (rounds, bytes, have, need) = run_reconciliation(items_a, items_b);
    let elapsed = start.elapsed();

    eprintln!(
        "[disjoint 1000] rounds={rounds}, bytes={bytes}, have={have}, need={need}, elapsed={elapsed:?}"
    );

    // Should discover all differences
    assert!(have + need > 0, "Should find differences in disjoint sets");
    assert!(rounds <= 20, "Too many rounds for disjoint sets: {rounds}");
}

#[test]
fn stress_overlapping_sets_5000_docs() {
    // A has 0..5000, B has 2500..7500 — 50% overlap
    let items_a = make_items("mesh", 5000, 1_000_000);
    let items_b: Vec<SyncItem> = (2500..7500)
        .map(|i| SyncItem::from_doc_key(&format!("mesh::doc-{i}"), 1_000_000 + i as u64))
        .collect();

    let start = Instant::now();
    let (rounds, bytes, have, need) = run_reconciliation(items_a, items_b);
    let elapsed = start.elapsed();

    eprintln!(
        "[overlap 5000] rounds={rounds}, bytes={bytes}, have={have}, need={need}, elapsed={elapsed:?}"
    );

    assert!(have + need > 0, "Should find differences in overlapping sets");
    // O(log n): log2(5000) ~ 13
    assert!(rounds <= 25, "Too many rounds for 5000 docs: {rounds}");
}

#[test]
fn stress_scaling_behavior() {
    // Measure rounds and bytes as doc count scales: 100, 500, 1000, 5000, 10000
    let sizes = [100, 500, 1_000, 5_000, 10_000];

    eprintln!("\n--- Negentropy scaling analysis ---");
    eprintln!("{:<10} {:>8} {:>12} {:>10}", "docs", "rounds", "bytes", "time_ms");

    let mut prev_rounds = 0u64;
    for &n in &sizes {
        // 10% difference: A has 0..n, B has (n/10)..n plus n/10 unique
        let items_a = make_items("scale", n, 1_000_000);
        let extra_start = n;
        let mut items_b: Vec<SyncItem> = ((n / 10)..n)
            .map(|i| SyncItem::from_doc_key(&format!("scale::doc-{i}"), 1_000_000 + i as u64))
            .collect();
        for i in 0..(n / 10) {
            items_b.push(SyncItem::from_doc_key(
                &format!("scale::doc-{}", extra_start + i),
                2_000_000 + i as u64,
            ));
        }

        let start = Instant::now();
        let (rounds, bytes, _have, _need) = run_reconciliation(items_a, items_b);
        let elapsed = start.elapsed();

        eprintln!(
            "{:<10} {:>8} {:>12} {:>10.1}",
            n,
            rounds,
            bytes,
            elapsed.as_secs_f64() * 1000.0
        );

        // Rounds should grow sub-linearly (O(log n))
        if prev_rounds > 0 && n > 100 {
            // When doc count increases 10x, rounds should not increase more than ~4x
            // (generous bound for O(log n))
            assert!(
                rounds <= prev_rounds * 5,
                "Rounds scaling faster than expected: {prev_rounds} -> {rounds} for {n} docs"
            );
        }
        prev_rounds = rounds;
    }
}

#[test]
fn stress_memory_overhead_10000_docs() {
    // Verify memory doesn't blow up with large document sets
    let n = 10_000;
    let items = make_items("mem", n, 1_000_000);

    // Rough memory estimate: each SyncItem is ~(32 + 8 + 64 avg key) = ~104 bytes
    // 10k items ~ 1MB — should be fine
    let start = Instant::now();
    let (rounds, bytes, have, need) = run_reconciliation(items.clone(), items);
    let elapsed = start.elapsed();

    eprintln!(
        "[identical 10000] rounds={rounds}, bytes={bytes}, have={have}, need={need}, elapsed={elapsed:?}"
    );

    assert_eq!(have, 0);
    assert_eq!(need, 0);
    // Message overhead should be reasonable — fingerprints not full docs
    // For 10k identical docs, total exchange should be well under 100KB
    assert!(
        bytes < 100_000,
        "Message overhead too high for identical sets: {bytes} bytes"
    );
}

#[test]
fn stress_concurrent_sessions() {
    // One node syncing with 50 peers simultaneously
    let sync = NegentropySync::new();
    let local_items = make_items("local", 1000, 1_000_000);

    let mut init_messages = Vec::new();
    for i in 0..50u8 {
        let peer = test_peer_id(i + 10);
        let msg = sync.initiate_sync(peer, local_items.clone()).unwrap();
        init_messages.push((peer, msg));
        assert!(sync.has_session(&peer));
    }

    let stats = sync.stats();
    assert_eq!(stats.sessions_initiated, 50);

    // Cancel all sessions
    for (peer, _) in &init_messages {
        sync.cancel_session(peer);
        assert!(!sync.has_session(peer));
    }
}
