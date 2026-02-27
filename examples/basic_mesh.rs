//! Basic mesh setup — the "hello world" of peat-mesh.
//!
//! Run with: `cargo run --example basic_mesh`

use peat_mesh::{
    security::{DeviceKeypair, FormationKey},
    MeshConfig, MeshDiscoveryConfig, PeatMesh, PeatMeshBuilder, PeatMeshEvent,
};
use std::time::Duration;

fn main() {
    // 1. Configure the mesh node.
    let config = MeshConfig {
        node_id: Some("example-node-1".into()),
        discovery: MeshDiscoveryConfig {
            mdns_enabled: true,
            service_name: "peat-example".into(),
            interval: Duration::from_secs(15),
        },
        ..Default::default()
    };

    // 2. Generate cryptographic identity.
    let device_keypair = DeviceKeypair::generate();
    let formation_secret = FormationKey::generate_secret();
    let formation_key =
        FormationKey::from_base64("example-formation", &formation_secret).expect("valid key");

    println!("Device ID: {}", device_keypair.device_id());
    println!("Formation: {}", formation_key.formation_id());

    // 3. Build the mesh.
    let mesh: PeatMesh = PeatMeshBuilder::new(config)
        .with_device_keypair(device_keypair)
        .with_formation_key(formation_key)
        .build();

    // 4. Subscribe to events before starting.
    let mut events = mesh.subscribe_events();

    // 5. Start the mesh.
    mesh.start().expect("failed to start mesh");

    let status = mesh.status();
    println!(
        "Mesh running — node={}, state={:?}, peers={}",
        status.node_id, status.state, status.peer_count
    );

    // 6. Spawn event listener.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let event_task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => match event {
                        PeatMeshEvent::StateChanged(state) => {
                            println!("Event: state changed to {:?}", state);
                        }
                        PeatMeshEvent::PeerJoined(peer) => {
                            println!("Event: peer joined — {}", peer.as_str());
                        }
                        PeatMeshEvent::PeerLeft(peer) => {
                            println!("Event: peer left — {}", peer.as_str());
                        }
                        PeatMeshEvent::TopologyChanged(topo) => {
                            println!("Event: topology changed — {:?}", topo);
                        }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        println!("Warning: missed {} events", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // 7. Wait for Ctrl+C, then shut down.
        println!("Press Ctrl+C to stop...");
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl+c");

        event_task.abort();
    });

    // 8. Graceful shutdown.
    mesh.stop().expect("failed to stop mesh");
    println!("Mesh stopped.");
}
