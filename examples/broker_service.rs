//! HTTP/WS broker service — expose mesh state over REST and WebSocket.
//!
//! Run with: `cargo run --example broker_service --features automerge-backend,broker`

use peat_mesh::{
    broker::{Broker, BrokerConfig},
    security::{DeviceKeypair, FormationKey},
    MeshConfig, MeshDiscoveryConfig, PeatMeshBuilder,
};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    // Initialize tracing for request logs.
    tracing_subscriber::fmt().with_env_filter("info").init();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // 1. Build the mesh.
        let config = MeshConfig {
            node_id: Some("broker-example".into()),
            discovery: MeshDiscoveryConfig {
                mdns_enabled: true,
                service_name: "peat-broker-example".into(),
                interval: Duration::from_secs(30),
            },
            ..Default::default()
        };

        let device_keypair = DeviceKeypair::generate();
        let formation_secret = FormationKey::generate_secret();
        let formation_key =
            FormationKey::from_base64("broker-formation", &formation_secret).expect("valid key");

        let mesh = PeatMeshBuilder::new(config)
            .with_device_keypair(device_keypair)
            .with_formation_key(formation_key)
            .build();

        mesh.start().expect("failed to start mesh");
        println!("Mesh started: node_id={}", mesh.node_id());

        // 2. Configure and start the broker.
        let broker_config = BrokerConfig {
            bind_addr: "127.0.0.1:8081".parse().unwrap(),
            timeout_secs: 30,
        };

        let mesh = Arc::new(mesh);
        let broker = Broker::new(mesh.clone()).with_config(broker_config.clone());

        println!("\nBroker endpoints:");
        println!("  GET  http://{}/api/v1/health", broker_config.bind_addr);
        println!("  GET  http://{}/api/v1/ready", broker_config.bind_addr);
        println!("  GET  http://{}/api/v1/node", broker_config.bind_addr);
        println!("  GET  http://{}/api/v1/peers", broker_config.bind_addr);
        println!("  GET  http://{}/api/v1/topology", broker_config.bind_addr);
        println!(
            "  GET  http://{}/api/v1/documents/:collection",
            broker_config.bind_addr
        );
        println!("  WS   ws://{}/api/v1/ws", broker_config.bind_addr);
        println!("\nPress Ctrl+C to stop...\n");

        // 3. Serve until Ctrl+C.
        tokio::select! {
            result = broker.serve() => {
                if let Err(e) = result {
                    eprintln!("Broker error: {}", e);
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nShutting down...");
            }
        }

        // 4. Graceful shutdown.
        mesh.stop().expect("failed to stop mesh");
        println!("Done.");
    });
}
