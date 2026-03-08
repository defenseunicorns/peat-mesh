//! peat-mesh-node — Kubernetes-ready mesh node binary.
//!
//! Reads configuration from environment variables, builds an `PeatMesh`
//! instance with deterministic keypair and Kubernetes discovery, and
//! serves the broker HTTP/WS API until SIGTERM/SIGINT.

use peat_mesh::broker::{Broker, BrokerConfig, OtaAppState};
use peat_mesh::config::{IrohConfig, MeshConfig};
use peat_mesh::discovery::{KubernetesDiscovery, KubernetesDiscoveryConfig};
use peat_mesh::mesh::PeatMeshBuilder;
use peat_mesh::peer_connector::PeerConnector;
use peat_mesh::qos::{start_periodic_gc, DeletionPolicyRegistry, GarbageCollector, GcConfig};
use peat_mesh::security::{DeviceKeypair, FormationKey};
use peat_mesh::storage::{
    AutomergeStore, AutomergeSyncCoordinator, MeshSyncTransport, NetworkedIrohBlobStore,
    SyncChannelManager, SyncProtocolHandler, SyncTransport, CAP_AUTOMERGE_ALPN,
};
use peat_mesh::transport::{
    LiteMeshTransport, LiteMessageType, LiteTransportConfig, MeshTransport, OtaSender,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, warn};

fn main() -> anyhow::Result<()> {
    // Install rustls crypto provider (required by kube's rustls-tls)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Initialize tracing from RUST_LOG (default: info,peat_mesh=debug)
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,peat_mesh=debug".to_string());
    tracing_subscriber::fmt().with_env_filter(filter).init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // ── Required env vars ────────────────────────────────────────
    let formation_secret = std::env::var("PEAT_FORMATION_SECRET")
        .map_err(|_| anyhow::anyhow!("PEAT_FORMATION_SECRET is required"))?;

    // ── Optional env vars ────────────────────────────────────────
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "peat-mesh-0".to_string());
    let discovery_mode =
        std::env::var("PEAT_DISCOVERY").unwrap_or_else(|_| "kubernetes".to_string());
    let broker_port: u16 = std::env::var("PEAT_BROKER_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8081);
    let iroh_bind_port: u16 = std::env::var("PEAT_IROH_BIND_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(11204);
    let relay_urls: Vec<String> = std::env::var("PEAT_IROH_RELAY_URLS")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    info!(
        hostname = %hostname,
        discovery = %discovery_mode,
        broker_port = broker_port,
        iroh_bind_port = iroh_bind_port,
        "Starting peat-mesh-node"
    );

    // ── Formation key ────────────────────────────────────────────
    let formation_key = FormationKey::from_base64("peat", &formation_secret)
        .map_err(|e| anyhow::anyhow!("Invalid PEAT_FORMATION_SECRET: {}", e))?;

    // ── Deterministic keypair from formation secret + hostname ───
    let seed = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        formation_secret.trim(),
    )
    .map_err(|e| anyhow::anyhow!("Invalid base64 in PEAT_FORMATION_SECRET: {}", e))?;

    // ── Derive deterministic Iroh secret key ─────────────────────
    let iroh_key = {
        use hkdf::Hkdf;
        use sha2::Sha256;
        let hk = Hkdf::<Sha256>::new(None, &seed);
        let mut key = [0u8; 32];
        hk.expand(format!("iroh:{}", hostname).as_bytes(), &mut key)
            .map_err(|e| anyhow::anyhow!("HKDF expand for Iroh key failed: {}", e))?;
        key
    };

    // ── Mesh config ──────────────────────────────────────────────
    let mesh_config = MeshConfig {
        node_id: Some(hostname.clone()),
        iroh: IrohConfig {
            bind_addr: Some(SocketAddr::from(([0, 0, 0, 0], iroh_bind_port))),
            relay_urls,
            secret_key: Some(iroh_key),
            ..Default::default()
        },
        ..Default::default()
    };

    // ── Discovery strategy ───────────────────────────────────────
    let mut discovery: Box<dyn peat_mesh::discovery::DiscoveryStrategy> =
        match discovery_mode.as_str() {
            "kubernetes" | "k8s" => {
                info!("Using Kubernetes EndpointSlice discovery");
                Box::new(KubernetesDiscovery::new(
                    KubernetesDiscoveryConfig::default(),
                ))
            }
            "mdns" => {
                info!("Using mDNS discovery");
                Box::new(
                    peat_mesh::discovery::MdnsDiscovery::new()
                        .map_err(|e| anyhow::anyhow!("mDNS discovery init failed: {}", e))?,
                )
            }
            other => {
                anyhow::bail!("Unknown PEAT_DISCOVERY mode: {}", other);
            }
        };

    // ── Take discovery event stream (must be before start) ───────
    let event_stream = discovery
        .event_stream()
        .map_err(|e| anyhow::anyhow!("Failed to get discovery event stream: {}", e))?;

    // ── Start discovery ──────────────────────────────────────────
    discovery
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start discovery: {}", e))?;
    info!("Discovery started");

    // ── Build Iroh endpoint ──────────────────────────────────────
    let (endpoint, static_provider) = NetworkedIrohBlobStore::build_endpoint(&mesh_config.iroh)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build Iroh endpoint: {}", e))?;

    info!(
        iroh_endpoint_id = %endpoint.id().fmt_short(),
        "Iroh endpoint ready"
    );

    // ── Automerge document store ────────────────────────────────
    let data_dir = std::env::var("PEAT_DATA_DIR").unwrap_or_else(|_| "/data".to_string());
    let automerge_store = Arc::new(
        AutomergeStore::open(format!("{}/automerge", data_dir))
            .map_err(|e| anyhow::anyhow!("Failed to open AutomergeStore: {}", e))?,
    );

    // ── Garbage collector (ADR-034 Phase 3) ────────────────────
    let gc_policy_registry = Arc::new(DeletionPolicyRegistry::with_defaults());
    let gc = Arc::new(GarbageCollector::with_policy_registry(
        automerge_store.clone(),
        gc_policy_registry,
        GcConfig::default(), // 5 minute interval
    ));
    let gc_handle = start_periodic_gc(gc.clone());
    info!("Garbage collector started (interval=5m)");

    // ── Sync transport (shares endpoint with blob store) ────────
    let sync_transport = Arc::new(MeshSyncTransport::new(endpoint.clone()));

    // ── Sync coordinator ────────────────────────────────────────
    let coordinator = Arc::new(AutomergeSyncCoordinator::new(
        automerge_store.clone(),
        sync_transport.clone() as Arc<dyn SyncTransport>,
    ));

    // ── SyncChannelManager ──────────────────────────────────────
    let channel_manager = Arc::new(SyncChannelManager::new(
        sync_transport.clone() as Arc<dyn SyncTransport>,
        coordinator.clone(),
    ));
    coordinator.set_channel_manager(channel_manager);

    // ── Sync protocol handler (for incoming QUIC connections) ───
    let sync_handler = SyncProtocolHandler::new(sync_transport.clone(), coordinator.clone());

    // ── Create networked blob store with sync protocol ──────────
    let blob_dir = std::env::temp_dir().join(format!("peat_iroh_blobs_{}", hostname));
    let blob_store = NetworkedIrohBlobStore::from_endpoint_with_protocols(
        blob_dir,
        endpoint,
        static_provider,
        vec![(CAP_AUTOMERGE_ALPN, Box::new(sync_handler))],
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to create networked blob store: {}", e))?;

    info!(
        iroh_endpoint_id = %blob_store.endpoint_id().fmt_short(),
        "Iroh blob store ready (blobs + automerge sync)"
    );

    // ── Build mesh ───────────────────────────────────────────────
    let mesh = PeatMeshBuilder::new(mesh_config)
        .with_device_keypair_from_seed(&seed, &hostname)
        .map_err(|e| anyhow::anyhow!("Keypair derivation failed: {}", e))?
        .with_formation_key(formation_key)
        .with_discovery(discovery)
        .build();

    mesh.start()
        .map_err(|e| anyhow::anyhow!("Failed to start mesh: {}", e))?;

    let device_id = mesh
        .device_keypair()
        .map(|kp| kp.device_id().to_hex())
        .unwrap_or_else(|| "unknown".to_string());
    info!(node_id = %mesh.node_id(), device_id = %device_id, "Mesh started");

    // ── Spawn PeerConnector ──────────────────────────────────────
    let connector = PeerConnector::new(seed.clone(), blob_store.clone());
    let _connector_handle = connector.run(event_stream);

    // ── Spawn sync polling task ─────────────────────────────────
    // Periodically sync all documents with connected peers.
    // K8s pods are flat (no hierarchy), so broadcast to all is correct.
    let (sync_cancel_tx, mut sync_cancel_rx) = tokio::sync::watch::channel(false);
    let _sync_poll_handle = {
        let coordinator = coordinator.clone();
        let transport = sync_transport.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = sync_cancel_rx.changed() => {
                        info!("Sync polling task shutting down");
                        break;
                    }
                }
                let peers = transport.connected_peers();
                for peer_id in peers {
                    if let Err(e) = coordinator.sync_all_documents_with_peer(peer_id).await {
                        warn!(
                            peer = %peer_id.fmt_short(),
                            error = %e,
                            "Failed to sync documents with peer"
                        );
                    }
                    // Exchange tombstones so deletions propagate (ADR-034)
                    if let Err(e) = coordinator.sync_tombstones_with_peer(peer_id).await {
                        warn!(
                            peer = %peer_id.fmt_short(),
                            error = %e,
                            "Failed to sync tombstones with peer"
                        );
                    }
                }
            }
        })
    };

    // ── Peat-Lite transport + OTA sender ───────────────────────────
    let lite_port: u16 = std::env::var("PEAT_LITE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5555);

    let lite_config = LiteTransportConfig {
        listen_port: lite_port,
        broadcast_port: lite_port,
        ..Default::default()
    };

    // Use a node_id derived from hostname for the Lite transport
    let lite_node_id: u32 = {
        let mut hash: u32 = 0;
        for b in hostname.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(b as u32);
        }
        hash
    };

    let lite_transport = Arc::new(LiteMeshTransport::new(lite_config, lite_node_id));
    if let Err(e) = lite_transport.start().await {
        warn!(
            "Failed to start Lite transport: {} (OTA will be unavailable)",
            e
        );
    } else {
        info!(port = lite_port, "Peat-Lite transport started");
    }

    // Derive OTA signing keypair from formation secret
    let ota_keypair = DeviceKeypair::from_seed(&seed, "peat-ota-signing-v1")
        .map_err(|e| anyhow::anyhow!("OTA keypair derivation failed: {}", e))?;
    info!(
        ota_signing_pubkey = %hex::encode(ota_keypair.public_key_bytes()),
        "OTA signing keypair derived (use this pubkey for peat-lite builds)"
    );
    let ota_sender = Arc::new(OtaSender::new(
        lite_transport.clone(),
        Some(Arc::new(ota_keypair)),
    ));

    // Wire OTA callback: route OTA messages from Lite peers to OtaSender
    {
        let ota_sender_ref = ota_sender.clone();
        lite_transport.set_ota_callback(move |peer_id, msg_type, payload| {
            let sender = ota_sender_ref.clone();
            let peer = peer_id.to_string();
            let pl = payload.to_vec();
            // Spawn because the callback is synchronous but OtaSender methods are async
            tokio::spawn(async move {
                match msg_type {
                    LiteMessageType::OtaAccept => sender.handle_accept(&peer, &pl).await,
                    LiteMessageType::OtaAck => sender.handle_ack(&peer, &pl).await,
                    LiteMessageType::OtaResult => sender.handle_result(&peer, &pl).await,
                    LiteMessageType::OtaAbort => sender.handle_abort(&peer, &pl).await,
                    _ => {}
                }
            });
        });
    }

    // Spawn OTA sender tick task (retransmit/timeout management)
    let (ota_cancel_tx, mut ota_cancel_rx) = tokio::sync::watch::channel(false);
    let _ota_tick_handle = {
        let sender = ota_sender.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = ota_cancel_rx.changed() => {
                        info!("OTA tick task shutting down");
                        break;
                    }
                }
                sender.tick().await;
            }
        })
    };

    // ── Broker HTTP server ───────────────────────────────────────
    let mesh = Arc::new(mesh);
    let broker_config = BrokerConfig {
        bind_addr: SocketAddr::from(([0, 0, 0, 0], broker_port)),
        ..Default::default()
    };
    let store_adapter = peat_mesh::broker::StoreBrokerAdapter::new(automerge_store.clone());
    let composite_state = Arc::new(peat_mesh::broker::CompositeBrokerState::new(
        mesh.clone() as Arc<dyn peat_mesh::broker::state::MeshBrokerState>,
        store_adapter,
    ));

    let ota_app_state = Arc::new(OtaAppState {
        sender: ota_sender.clone(),
    });

    let broker = Broker::new(composite_state as Arc<dyn peat_mesh::broker::state::MeshBrokerState>)
        .with_config(broker_config);
    let router = broker.build_router_with_ota(ota_app_state);

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], broker_port)))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind broker port {}: {}", broker_port, e))?;

    info!(addr = %listener.local_addr()?, "Broker listening");

    // ── Graceful shutdown on SIGTERM/SIGINT ───────────────────────
    let mesh_ref = mesh.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow::anyhow!("Broker server error: {}", e))?;

    info!("Shutting down...");
    let _ = sync_cancel_tx.send(true);
    let _ = ota_cancel_tx.send(true);
    gc.stop();
    gc_handle.abort();
    if let Err(e) = lite_transport.stop().await {
        error!("Error stopping Lite transport: {}", e);
    }
    if let Err(e) = blob_store.shutdown().await {
        error!("Error shutting down Iroh router: {}", e);
    }
    if let Err(e) = mesh_ref.stop() {
        error!("Error stopping mesh: {}", e);
    }
    info!("peat-mesh-node stopped");

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received SIGINT"),
        _ = terminate => info!("Received SIGTERM"),
    }
}
