//! Chaos tests for transport failover under DDIL conditions (Issue #7)
//!
//! Validates transport selection, health monitoring, and reconnection
//! under simulated degraded, disconnected, intermittent, and limited
//! bandwidth (DDIL) scenarios.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;

use peat_mesh::transport::{
    HealthMonitor, HeartbeatConfig, MeshConnection, MeshTransport, NodeId, PeerEventReceiver,
    Transport, TransportCapabilities, TransportInstance, TransportManager, TransportManagerConfig,
    TransportPolicy, TransportType,
};
use peat_mesh::transport::reconnection::{ReconnectionManager, ReconnectionPolicy};
use peat_mesh::transport::{Result, TransportError};

// =============================================================================
// Mock Transport with controllable availability
// =============================================================================

struct ChaosTransport {
    caps: TransportCapabilities,
    available: std::sync::atomic::AtomicBool,
    reachable_peers: Vec<NodeId>,
}

impl ChaosTransport {
    fn new(transport_type: TransportType, peers: Vec<NodeId>) -> Self {
        let caps = match transport_type {
            TransportType::Quic => TransportCapabilities {
                transport_type: TransportType::Quic,
                reliable: true,
                bidirectional: true,
                max_message_size: 65536,
                typical_latency_ms: 10,
                max_bandwidth_bps: 10_000_000,
                supports_broadcast: false,
                max_range_meters: 0,
                battery_impact: 20,
                requires_pairing: false,
            },
            TransportType::BluetoothLE => TransportCapabilities {
                transport_type: TransportType::BluetoothLE,
                reliable: true,
                bidirectional: true,
                max_message_size: 512,
                typical_latency_ms: 50,
                max_bandwidth_bps: 250_000,
                supports_broadcast: true,
                max_range_meters: 30,
                battery_impact: 40,
                requires_pairing: true,
            },
            _ => TransportCapabilities {
                transport_type,
                reliable: false,
                bidirectional: false,
                max_message_size: 1500,
                typical_latency_ms: 5,
                max_bandwidth_bps: 1_000_000,
                supports_broadcast: true,
                max_range_meters: 0,
                battery_impact: 10,
                requires_pairing: false,
            },
        };
        Self {
            caps,
            available: std::sync::atomic::AtomicBool::new(true),
            reachable_peers: peers,
        }
    }

    fn set_available(&self, available: bool) {
        self.available
            .store(available, std::sync::atomic::Ordering::SeqCst);
    }
}

struct MockConnection {
    peer_id: NodeId,
    connected_at: Instant,
}

impl MeshConnection for MockConnection {
    fn peer_id(&self) -> &NodeId {
        &self.peer_id
    }
    fn is_alive(&self) -> bool {
        true
    }
    fn connected_at(&self) -> Instant {
        self.connected_at
    }
}

#[async_trait]
impl MeshTransport for ChaosTransport {
    async fn start(&self) -> Result<()> {
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        Ok(())
    }
    async fn connect(&self, peer_id: &NodeId) -> Result<Box<dyn MeshConnection>> {
        if !self.available.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(TransportError::ConnectionFailed(
                "transport unavailable".into(),
            ));
        }
        if self.reachable_peers.contains(peer_id) {
            Ok(Box::new(MockConnection {
                peer_id: peer_id.clone(),
                connected_at: Instant::now(),
            }))
        } else {
            Err(TransportError::PeerNotFound(peer_id.to_string()))
        }
    }
    async fn disconnect(&self, _peer_id: &NodeId) -> Result<()> {
        Ok(())
    }
    fn get_connection(&self, _peer_id: &NodeId) -> Option<Box<dyn MeshConnection>> {
        None
    }
    fn peer_count(&self) -> usize {
        0
    }
    fn connected_peers(&self) -> Vec<NodeId> {
        vec![]
    }
    fn subscribe_peer_events(&self) -> PeerEventReceiver {
        let (_tx, rx) = mpsc::channel(1);
        rx
    }
}

impl Transport for ChaosTransport {
    fn capabilities(&self) -> &TransportCapabilities {
        &self.caps
    }
    fn is_available(&self) -> bool {
        self.available.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn can_reach(&self, peer_id: &NodeId) -> bool {
        self.is_available() && self.reachable_peers.contains(peer_id)
    }
}

// =============================================================================
// Tests
// =============================================================================

/// When QUIC drops, transport selection should fall back to BLE
#[test]
fn failover_quic_to_ble_on_link_drop() {
    let peer = NodeId::new("alpha".into());

    let quic = Arc::new(ChaosTransport::new(TransportType::Quic, vec![peer.clone()]));
    let ble = Arc::new(ChaosTransport::new(
        TransportType::BluetoothLE,
        vec![peer.clone()],
    ));

    let mut manager = TransportManager::new(TransportManagerConfig {
        enable_fallback: true,
        ..Default::default()
    });
    manager.register(quic.clone() as Arc<dyn Transport>);
    manager.register(ble.clone() as Arc<dyn Transport>);

    // Both available
    let available = manager.available_transports(&peer);
    assert!(available.contains(&TransportType::Quic));
    assert!(available.contains(&TransportType::BluetoothLE));

    // Drop QUIC
    quic.set_available(false);
    let available = manager.available_transports(&peer);
    assert!(!available.contains(&TransportType::Quic));
    assert!(available.contains(&TransportType::BluetoothLE));
    assert_eq!(available.len(), 1);
}

/// When QUIC recovers, it should be available again
#[test]
fn recovery_quic_after_link_restore() {
    let peer = NodeId::new("bravo".into());

    let quic = Arc::new(ChaosTransport::new(TransportType::Quic, vec![peer.clone()]));
    let ble = Arc::new(ChaosTransport::new(
        TransportType::BluetoothLE,
        vec![peer.clone()],
    ));

    let mut manager = TransportManager::new(TransportManagerConfig::default());
    manager.register(quic.clone() as Arc<dyn Transport>);
    manager.register(ble.clone() as Arc<dyn Transport>);

    quic.set_available(false);
    assert_eq!(manager.available_transports(&peer).len(), 1);

    quic.set_available(true);
    let available = manager.available_transports(&peer);
    assert_eq!(available.len(), 2);
    assert!(available.contains(&TransportType::Quic));
}

/// PACE policy failover: primary -> alternate -> total isolation
#[test]
fn pace_policy_cascading_failover() {
    let peer = NodeId::new("charlie".into());

    let primary = Arc::new(ChaosTransport::new(TransportType::Quic, vec![peer.clone()]));
    let alternate = Arc::new(ChaosTransport::new(
        TransportType::BluetoothLE,
        vec![peer.clone()],
    ));

    let policy = TransportPolicy::new("ddil-test")
        .primary(vec!["quic-primary"])
        .alternate(vec!["ble-backup"]);

    let manager = TransportManager::new(TransportManagerConfig::with_policy(policy));

    let primary_inst = TransportInstance::new(
        "quic-primary",
        TransportType::Quic,
        primary.capabilities().clone(),
    );
    let alt_inst = TransportInstance::new(
        "ble-backup",
        TransportType::BluetoothLE,
        alternate.capabilities().clone(),
    );

    manager.register_instance(primary_inst, primary.clone() as Arc<dyn Transport>);
    manager.register_instance(alt_inst, alternate.clone() as Arc<dyn Transport>);

    // Both available
    let available = manager.available_instance_ids();
    assert!(available.contains(&String::from("quic-primary")));
    assert!(available.contains(&String::from("ble-backup")));

    // Drop primary
    primary.set_available(false);
    let available = manager.available_instance_ids();
    assert!(!available.contains(&String::from("quic-primary")));
    assert!(available.contains(&String::from("ble-backup")));

    // Drop all
    alternate.set_available(false);
    assert!(manager.available_instance_ids().is_empty());
}

/// Health monitor detects degradation under simulated high latency
#[test]
fn health_monitor_degradation_under_high_latency() {
    let config = HeartbeatConfig {
        interval: Duration::from_millis(10),
        suspect_threshold: 3,
        dead_threshold: 6,
        degraded_rtt_threshold_ms: 50,
        degraded_loss_threshold_percent: 100, // only test RTT-based degradation
        enabled: true,
    };
    let monitor = HealthMonitor::new(config);
    let peer = NodeId::new("delta".into());
    monitor.start_monitoring(peer.clone());

    // Simulate high-latency pongs (> 50ms threshold)
    for _ in 0..3 {
        let seq = monitor.record_ping_sent(&peer).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        monitor.record_pong_received(&peer, seq);
    }
    monitor.check_timeouts();

    let health = monitor.get_health(&peer).unwrap();
    assert_eq!(
        health.state,
        peat_mesh::transport::ConnectionState::Degraded,
        "Should be degraded with RTT > 50ms, got {:?}",
        health.state
    );
}

/// Health monitor: intermittent connectivity (some pongs, some missed)
#[test]
fn health_monitor_intermittent_connectivity() {
    let config = HeartbeatConfig {
        interval: Duration::from_millis(10),
        suspect_threshold: 2,
        dead_threshold: 4,
        degraded_loss_threshold_percent: 100,
        degraded_rtt_threshold_ms: 1000,
        enabled: true,
    };
    let monitor = HealthMonitor::new(config);
    let peer = NodeId::new("echo".into());
    monitor.start_monitoring(peer.clone());

    // Round 1: respond
    let seq = monitor.record_ping_sent(&peer).unwrap();
    monitor.record_pong_received(&peer, seq);

    // Round 2: miss
    monitor.record_ping_sent(&peer);
    std::thread::sleep(Duration::from_millis(15));
    monitor.check_timeouts();

    // Round 3: respond (resets missed count)
    let seq = monitor.record_ping_sent(&peer).unwrap();
    monitor.record_pong_received(&peer, seq);

    let health = monitor.get_health(&peer).unwrap();
    assert_eq!(
        health.state,
        peat_mesh::transport::ConnectionState::Healthy
    );

    // Round 4+5: miss twice -> suspect
    monitor.record_ping_sent(&peer);
    std::thread::sleep(Duration::from_millis(15));
    monitor.check_timeouts();
    monitor.record_ping_sent(&peer);
    std::thread::sleep(Duration::from_millis(15));
    monitor.check_timeouts();

    let health = monitor.get_health(&peer).unwrap();
    assert_eq!(
        health.state,
        peat_mesh::transport::ConnectionState::Suspect
    );
}

/// Health monitor: full lifecycle Healthy -> Suspect -> Dead
#[test]
fn health_monitor_full_lifecycle() {
    let config = HeartbeatConfig {
        interval: Duration::from_millis(10),
        suspect_threshold: 2,
        dead_threshold: 4,
        degraded_loss_threshold_percent: 100,
        degraded_rtt_threshold_ms: 1000,
        enabled: true,
    };
    let monitor = HealthMonitor::new(config);
    let peer = NodeId::new("foxtrot".into());
    monitor.start_monitoring(peer.clone());

    // Drive to Dead (4 missed heartbeats)
    for _ in 0..4 {
        monitor.record_ping_sent(&peer);
        std::thread::sleep(Duration::from_millis(15));
        let dead = monitor.check_timeouts();
        if !dead.is_empty() {
            assert_eq!(dead[0], peer);
        }
    }

    let health = monitor.get_health(&peer).unwrap();
    assert_eq!(health.state, peat_mesh::transport::ConnectionState::Dead);

    // Dead peers should not be pinged
    assert!(!monitor.peers_needing_ping().contains(&peer));
}

/// Reconnection manager: exponential backoff under repeated failures
#[test]
fn reconnection_exponential_backoff() {
    let policy = ReconnectionPolicy {
        enabled: true,
        initial_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(10),
        backoff_multiplier: 2.0,
        max_retries: Some(5),
        jitter: 0.0,
    };
    let mut manager = ReconnectionManager::new(policy.clone());
    let peer = NodeId::new("golf".into());

    manager.schedule_reconnect(peer.clone(), true);

    // Verify backoff delays double each time
    for attempt in 0..5u32 {
        let delay = policy.calculate_delay(attempt);
        let expected_base = 100.0 * 2.0f64.powi(attempt as i32);
        let expected = expected_base.min(10_000.0);
        assert!(
            (delay.as_millis() as f64 - expected).abs() < 1.0,
            "attempt {attempt}: expected {expected}ms, got {}ms",
            delay.as_millis()
        );
    }

    // Exhaust retries
    for i in 0..5 {
        let should_retry = manager.failed(&peer, format!("connection refused attempt {i}"));
        if i < 4 {
            assert!(should_retry, "Should retry on attempt {i}");
        } else {
            assert!(!should_retry, "Should stop after max retries");
        }
    }
}

/// Reconnection manager: successful reconnect clears state
#[test]
fn reconnection_success_clears_state() {
    let mut manager = ReconnectionManager::new(ReconnectionPolicy::default());
    let peer = NodeId::new("hotel".into());

    manager.schedule_reconnect(peer.clone(), true);
    assert!(manager.is_pending(&peer));

    manager.failed(&peer, "timeout".into());
    manager.failed(&peer, "timeout".into());
    assert_eq!(manager.get_state(&peer).unwrap().attempts, 2);

    manager.reconnected(&peer);
    assert!(!manager.is_pending(&peer));
}

/// Simulate rapid transport flapping (up/down/up/down)
#[test]
fn transport_flapping_stability() {
    let peer = NodeId::new("india".into());
    let quic = Arc::new(ChaosTransport::new(TransportType::Quic, vec![peer.clone()]));
    let ble = Arc::new(ChaosTransport::new(
        TransportType::BluetoothLE,
        vec![peer.clone()],
    ));

    let mut manager = TransportManager::new(TransportManagerConfig::default());
    manager.register(quic.clone() as Arc<dyn Transport>);
    manager.register(ble.clone() as Arc<dyn Transport>);

    // Rapidly toggle QUIC 10 times
    for _ in 0..10 {
        quic.set_available(false);
        let a = manager.available_transports(&peer);
        assert!(!a.contains(&TransportType::Quic));
        assert!(a.contains(&TransportType::BluetoothLE));

        quic.set_available(true);
        let a = manager.available_transports(&peer);
        assert!(a.contains(&TransportType::Quic));
        assert!(a.contains(&TransportType::BluetoothLE));
    }
}

/// Multiple peers with independent health monitoring
#[test]
fn independent_peer_transport_failures() {
    let peer_a = NodeId::new("juliet-a".into());
    let peer_b = NodeId::new("juliet-b".into());

    let monitor = HealthMonitor::new(HeartbeatConfig {
        interval: Duration::from_millis(10),
        suspect_threshold: 2,
        dead_threshold: 4,
        degraded_loss_threshold_percent: 100,
        degraded_rtt_threshold_ms: 1000,
        enabled: true,
    });

    monitor.start_monitoring(peer_a.clone());
    monitor.start_monitoring(peer_b.clone());

    // Peer A goes unhealthy (2 missed heartbeats -> Suspect)
    for _ in 0..2 {
        monitor.record_ping_sent(&peer_a);
        std::thread::sleep(Duration::from_millis(15));
        monitor.check_timeouts();
    }

    // Peer B stays healthy
    let seq = monitor.record_ping_sent(&peer_b).unwrap();
    monitor.record_pong_received(&peer_b, seq);

    let health_a = monitor.get_health(&peer_a).unwrap();
    let health_b = monitor.get_health(&peer_b).unwrap();
    assert_eq!(
        health_a.state,
        peat_mesh::transport::ConnectionState::Suspect
    );
    assert_eq!(
        health_b.state,
        peat_mesh::transport::ConnectionState::Healthy
    );
}

/// Reconnection manager handles many peers under simultaneous failure
#[test]
fn mass_disconnection_reconnection() {
    let mut manager = ReconnectionManager::new(ReconnectionPolicy {
        initial_delay: Duration::from_millis(10),
        max_retries: Some(3),
        ..Default::default()
    });

    let peers: Vec<NodeId> = (0..50)
        .map(|i| NodeId::new(format!("kilo-{i}")))
        .collect();

    for peer in &peers {
        manager.schedule_reconnect(peer.clone(), true);
    }
    assert_eq!(manager.pending_count(), 50);

    std::thread::sleep(Duration::from_millis(15));

    let due = manager.due_reconnections();
    assert_eq!(due.len(), 50);

    // Half succeed, half fail
    for (i, peer) in peers.iter().enumerate() {
        if i % 2 == 0 {
            manager.reconnected(peer);
        } else {
            manager.failed(peer, "connection refused".into());
        }
    }

    assert_eq!(manager.pending_count(), 25);
}

/// Total isolation: all transports down simultaneously, then partial/full recovery
#[test]
fn total_isolation_all_transports_down() {
    let peer = NodeId::new("lima".into());

    let policy = TransportPolicy::new("isolation-test")
        .primary(vec!["quic-main"])
        .alternate(vec!["ble-fallback"]);

    let manager = TransportManager::new(TransportManagerConfig::with_policy(policy));

    let quic = Arc::new(ChaosTransport::new(TransportType::Quic, vec![peer.clone()]));
    let ble = Arc::new(ChaosTransport::new(
        TransportType::BluetoothLE,
        vec![peer.clone()],
    ));

    manager.register_instance(
        TransportInstance::new("quic-main", TransportType::Quic, quic.capabilities().clone()),
        quic.clone() as Arc<dyn Transport>,
    );
    manager.register_instance(
        TransportInstance::new(
            "ble-fallback",
            TransportType::BluetoothLE,
            ble.capabilities().clone(),
        ),
        ble.clone() as Arc<dyn Transport>,
    );

    assert_eq!(manager.available_instance_ids().len(), 2);

    // Total isolation
    quic.set_available(false);
    ble.set_available(false);
    assert!(manager.available_instance_ids().is_empty());
    assert!(manager.available_instances_for_peer(&peer).is_empty());

    // Partial recovery (BLE only)
    ble.set_available(true);
    let available = manager.available_instance_ids();
    assert_eq!(available.len(), 1);
    assert!(available.contains(&String::from("ble-fallback")));

    // Full recovery
    quic.set_available(true);
    assert_eq!(manager.available_instance_ids().len(), 2);
}
