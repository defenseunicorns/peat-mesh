# peat-mesh

Mesh networking library with CRDT sync, transport security, and topology management.

## Overview

peat-mesh provides the core mesh networking layer for the Peat protocol, including:

- **Transport** - Pluggable transports (UDP bypass, BLE) with connection health monitoring and reconnection
- **Security** - Ed25519 identity, X25519 key exchange, ChaCha20-Poly1305 encryption, formation keys
- **Discovery** - mDNS and static peer discovery with hybrid strategies
- **Topology** - Dynamic topology management with partition detection and autonomous operation
- **Routing** - Mesh routing with data aggregation and deduplication
- **Storage** - Automerge CRDT backend with Iroh P2P sync, negentropy set reconciliation, redb persistence, and streaming large-blob transfer
- **Beacon** - Geographic beacon broadcasting and observation with geohash indexing
- **QoS** - Bandwidth management, TTL, retention policies, sync modes, and garbage collection
- **Broker** - Optional HTTP/WebSocket service broker (Axum-based)

## Installation

```toml
[dependencies]
peat-mesh = "0.3.2"
```

### Feature flags

| Feature | Description |
|---------|-------------|
| `automerge-backend` | Automerge CRDT storage with Iroh P2P sync and redb persistence |
| `bluetooth` | BLE mesh transport via [peat-btle](https://crates.io/crates/peat-btle) |
| `broker` | HTTP/WebSocket service broker (Axum) |
| `kubernetes` | Kubernetes peer discovery via EndpointSlice API ([guide](docs/kubernetes.md)) |
| `node` | All-in-one feature for the `peat-mesh-node` binary (includes broker, kubernetes, automerge-backend) |
| `lite-bridge` | UDP bridge for Peat-Lite embedded devices (OTA, telemetry) |

```toml
# Example with features
peat-mesh = { version = "0.1.0", features = ["automerge-backend", "bluetooth"] }
```

## Quick start

```rust
use peat_mesh::{PeatMeshBuilder, MeshConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mesh = PeatMeshBuilder::new()
        .with_config(MeshConfig::default())
        .build()
        .await?;

    // mesh is ready for peer discovery and data sync
    Ok(())
}
```

## Streaming Transfer

peat-mesh includes a streaming large-blob transfer module for bounded-memory file transfer across DDIL links. Transfers use O(chunk_size) memory regardless of blob size, with incremental SHA256 verification and resumable sessions via periodic checkpointing.

```rust
use peat_mesh::storage::{StreamingTransferConfig, TransferCheckpoint};

// Pre-built profiles for different network conditions
let config = StreamingTransferConfig::tactical(); // 256 KiB chunks, 30s checkpoint interval
// Also available: StreamingTransferConfig::datacenter() and ::edge()

// Resume from a previous checkpoint
let checkpoint = TransferCheckpoint::load("transfer-session.json")?;
```

| Profile | Chunk Size | Checkpoint Interval | Use Case |
|---------|-----------|-------------------|----------|
| `datacenter()` | 1 MiB | 60s | High-bandwidth, reliable links |
| `tactical()` | 256 KiB | 30s | Intermittent tactical networks |
| `edge()` | 64 KiB | 10s | Low-bandwidth edge/BTLE links |

## Kubernetes Deployment

peat-mesh includes a binary target (`peat-mesh-node`), Dockerfile, and Helm chart for Kubernetes deployment.

```bash
# Build the binary
cargo build --release --bin peat-mesh-node --features node

# Build Docker image
docker build -t peat-mesh-node:latest -f deploy/Dockerfile .

# Deploy to k3d (local Kubernetes)
k3d cluster create peat-alpha
k3d image import peat-mesh-node:latest -c peat-alpha
helm install peat-mesh deploy/helm/peat-mesh \
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

- **GitHub**: [peat-mesh](https://github.com/defenseunicorns/peat-mesh)
- **crates.io**: [peat-mesh](https://crates.io/crates/peat-mesh)

## License

Apache-2.0

## OTA Firmware Updates

peat-mesh nodes can push firmware updates to Peat-Lite ESP32 devices over UDP:

```bash
# Via HTTP API (when running as peat-mesh-node)
curl -X POST http://localhost:3000/api/v1/ota/<peer_id> \
  -F "firmware=@firmware.bin" -F "version=0.2.0"

# Check progress
curl http://localhost:3000/api/v1/ota/<peer_id>/status
```

The OTA sender implements stop-and-wait reliable transfer with SHA256 verification and optional Ed25519 signing. See [ADR-047](https://github.com/defenseunicorns/peat-mesh/blob/main/docs/adr/047-firmware-ota-distribution.md) for protocol details.
