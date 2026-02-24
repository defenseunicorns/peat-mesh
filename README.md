# eche-mesh

Mesh networking library with CRDT sync, transport security, and topology management.

## Overview

eche-mesh provides the core mesh networking layer for the Eche protocol, including:

- **Transport** - Pluggable transports (UDP bypass, BLE) with connection health monitoring and reconnection
- **Security** - Ed25519 identity, X25519 key exchange, ChaCha20-Poly1305 encryption, formation keys
- **Discovery** - mDNS and static peer discovery with hybrid strategies
- **Topology** - Dynamic topology management with partition detection and autonomous operation
- **Routing** - Mesh routing with data aggregation and deduplication
- **Storage** - Automerge CRDT backend with Iroh P2P sync, negentropy set reconciliation, and redb persistence
- **Beacon** - Geographic beacon broadcasting and observation with geohash indexing
- **QoS** - Bandwidth management, TTL, retention policies, sync modes, and garbage collection
- **Broker** - Optional HTTP/WebSocket service broker (Axum-based)

## Installation

```toml
[dependencies]
eche-mesh = "0.1.0"
```

### Feature flags

| Feature | Description |
|---------|-------------|
| `automerge-backend` | Automerge CRDT storage with Iroh P2P sync and redb persistence |
| `bluetooth` | BLE mesh transport via [eche-btle](https://crates.io/crates/eche-btle) |
| `broker` | HTTP/WebSocket service broker (Axum) |
| `kubernetes` | Kubernetes peer discovery via EndpointSlice API ([guide](docs/kubernetes.md)) |
| `node` | All-in-one feature for the `eche-mesh-node` binary (includes broker, kubernetes, automerge-backend) |
| `lite-bridge` | UDP bridge for Eche-Lite embedded devices (OTA, telemetry) |

```toml
# Example with features
eche-mesh = { version = "0.1.0", features = ["automerge-backend", "bluetooth"] }
```

## Quick start

```rust
use eche_mesh::{EcheMeshBuilder, MeshConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mesh = EcheMeshBuilder::new()
        .with_config(MeshConfig::default())
        .build()
        .await?;

    // mesh is ready for peer discovery and data sync
    Ok(())
}
```

## Kubernetes Deployment

eche-mesh includes a binary target (`eche-mesh-node`), Dockerfile, and Helm chart for Kubernetes deployment.

```bash
# Build the binary
cargo build --release --bin eche-mesh-node --features node

# Build Docker image
docker build -t eche-mesh-node:latest -f deploy/Dockerfile .

# Deploy to k3d (local Kubernetes)
k3d cluster create eche-alpha
k3d image import eche-mesh-node:latest -c eche-alpha
helm install eche-mesh deploy/helm/eche-mesh \
  --set "formationSecret=$(openssl rand -base64 32)" \
  --set replicaCount=3
```

See the full [Deployment Guide](docs/deployment.md) for details on configuration, verification, cluster lifecycle, and multi-cluster federation testing.

## Development

```bash
# Build (default features)
cargo build

# Build with all optional features
cargo build --features automerge-backend,bluetooth,broker

# Run tests
cargo test

# Clippy
cargo clippy -- -D warnings
```

## CI

CI runs via [GOA](https://github.com/radicle-dev/goa) (GitOps Agent) on every Radicle patch:

1. **Format** - `cargo fmt --check`
2. **Clippy** - `cargo clippy -- -D warnings`
3. **Tests** - `cargo test`
4. **Feature builds** - `automerge-backend`, `bluetooth`, `broker`

Results are posted as patch reviews (accept/reject). Community patches require a delegate "ok-to-test" comment before CI runs.

### Manual CI

```bash
# Run the full CI pipeline locally
bash .goa
```

## Source

- **GitHub**: [eche-mesh](https://github.com/defenseunicorns/eche-mesh)
- **crates.io**: [eche-mesh](https://crates.io/crates/eche-mesh)

## License

Apache-2.0

## OTA Firmware Updates

eche-mesh nodes can push firmware updates to Eche-Lite ESP32 devices over UDP:

```bash
# Via HTTP API (when running as eche-mesh-node)
curl -X POST http://localhost:3000/api/v1/ota/<peer_id> \
  -F "firmware=@firmware.bin" -F "version=0.2.0"

# Check progress
curl http://localhost:3000/api/v1/ota/<peer_id>/status
```

The OTA sender implements stop-and-wait reliable transfer with SHA256 verification and optional Ed25519 signing. See [ADR-047](https://github.com/defenseunicorns/eche-mesh/blob/main/docs/adr/047-firmware-ota-distribution.md) for protocol details.
