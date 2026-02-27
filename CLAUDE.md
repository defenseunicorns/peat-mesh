# Claude Code Project Guide - peat-mesh

## Project Overview

peat-mesh is a standalone mesh networking library providing P2P topology, CRDT synchronization (Automerge + Iroh), pluggable transport, peer discovery, and a Kubernetes-ready binary. It is published on crates.io and consumed by the main PEAT workspace as `peat-mesh = "0.3.1"`.

**GitHub**: `defenseunicorns/peat-mesh`
**Radicle**: `rad:z2Jq9dpxMjf1H17JTT1D89RnvP4kQ`

## Build Commands

```bash
# Library (no features — minimal mesh)
cargo build
cargo test

# With Automerge CRDT sync
cargo build --features automerge-backend
cargo test --features automerge-backend

# With HTTP/WS broker
cargo build --features automerge-backend,broker

# Full node binary (all features)
cargo build --features node

# CI checks
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

## Feature Matrix

| Feature | What it enables |
|---------|----------------|
| (none) | Transport, discovery, security, routing, hierarchy, beacon, QoS |
| `lite-bridge` | Peat-Lite ESP32 bridge (UDP relay + OTA distribution) |
| `bluetooth` | BLE mesh transport (wraps peat-btle) |
| `automerge-backend` | Automerge CRDT store, Iroh P2P blobs, redb persistence, negentropy sync |
| `broker` | HTTP/WS API (Axum — REST + WebSocket streaming) |
| `kubernetes` | K8s EndpointSlice peer discovery |
| `node` | All of the above — builds `peat-mesh-node` binary |

## Module Map

```
src/
├── lib.rs              # Public API facade (re-exports)
├── mesh.rs             # PeatMesh / PeatMeshBuilder (lifecycle state machine)
├── config.rs           # MeshConfig, SecurityConfig, IrohConfig
│
├── transport/          # Pluggable transport abstraction
│   ├── mod.rs          #   NodeId, MeshTransport/MeshConnection traits
│   ├── bypass.rs       #   UDP bypass with health monitoring
│   ├── manager.rs      #   TransportManager (multi-transport routing)
│   ├── lite.rs         #   Peat-Lite bridge [lite-bridge]
│   ├── lite_ota.rs     #   OTA firmware distribution [lite-bridge]
│   └── btle.rs         #   BLE mesh transport [bluetooth]
│
├── discovery/          # Peer discovery strategies
│   ├── mdns.rs         #   mDNS/DNS-SD (local network)
│   ├── static_config.rs #  TOML-based static peers
│   ├── hybrid.rs       #   Composite (multiple strategies)
│   └── kubernetes.rs   #   K8s EndpointSlice [kubernetes]
│
├── security/           # Cryptographic primitives
│   ├── keypair.rs      #   DeviceKeypair (Ed25519, DID-like identity)
│   ├── encryption.rs   #   X25519 key exchange + ChaCha20-Poly1305
│   ├── formation_key.rs #  HMAC-SHA256 pre-shared key auth
│   └── callsign.rs     #   NATO phonetic callsign generator
│
├── topology/           # Topology formation & management
│   ├── builder.rs      #   TopologyBuilder, state machine
│   ├── selection.rs    #   PeerSelector (parent selection)
│   ├── autonomous.rs   #   Headless autonomous operation
│   └── partition.rs    #   Partition detection & recovery
│
├── routing/            # Hierarchy-aware mesh routing
│   ├── router.rs       #   SelectiveRouter (respects leader/member roles)
│   └── aggregator.rs   #   Data aggregation trait
│
├── hierarchy/          # Role & level assignment
│   ├── static_strategy.rs  # Fixed assignment
│   ├── dynamic_strategy.rs # Capability-based election
│   └── hybrid_strategy.rs  # Static + dynamic
│
├── beacon/             # Geographic beacon broadcasting
│   ├── broadcaster.rs  #   Publishes node position/capabilities
│   ├── observer.rs     #   Collects peer beacons for topology
│   └── janitor.rs      #   Stale beacon cleanup
│
├── qos/                # Quality of Service
│   ├── mod.rs          #   QoSClass (P1-P5), policies
│   ├── retention.rs    #   Per-class retention tracking
│   ├── deletion.rs     #   Tombstone-based deletion
│   ├── garbage_collection.rs # Periodic GC
│   ├── eviction.rs     #   Document eviction under pressure
│   └── bandwidth.rs    #   TokenBucket quota enforcement
│
├── storage/            # Storage backends [automerge-backend]
│   ├── automerge_store.rs    # CRDT document store
│   ├── iroh_blob_store.rs    # P2P blob sync
│   ├── automerge_sync.rs     # Sync coordinator (batching, direction)
│   ├── mesh_sync_transport.rs # Integrates sync into mesh transport
│   ├── negentropy_sync.rs    # Set reconciliation (O(log N))
│   └── ttl_manager.rs        # TTL enforcement
│
├── sync/               # Generic sync abstraction
│   ├── traits.rs       #   SyncBackend trait
│   └── in_memory.rs    #   No-persist reference impl
│
└── broker/             # HTTP/WS API [broker]
    ├── server.rs       #   Axum server + config
    ├── routes.rs       #   REST /api/v1/*
    ├── ws.rs           #   WebSocket event streaming
    └── ota_routes.rs   #   /api/v1/ota/* firmware endpoints
```

## Binary: peat-mesh-node

Built with `cargo build --features node`. Kubernetes-ready all-in-one mesh node.

### Environment Variables

| Variable | Required | Default | Purpose |
|----------|----------|---------|---------|
| `PEAT_FORMATION_SECRET` | Yes | — | Base64 seed for deterministic keypairs |
| `PEAT_DISCOVERY` | No | `kubernetes` | `kubernetes` or `mdns` |
| `PEAT_DATA_DIR` | No | `/data` | Automerge storage root |
| `PEAT_BROKER_PORT` | No | `8081` | HTTP/WS listen port |
| `PEAT_IROH_BIND_PORT` | No | `11204` | Iroh QUIC bind port |
| `RUST_LOG` | No | `info,peat_mesh=debug` | Tracing filter |

### Kubernetes Deployment

```bash
# Build image
docker build -t peat-mesh-node:latest -f deploy/Dockerfile .

# Local k3d cluster
k3d cluster create peat --no-lb
k3d image import peat-mesh-node:latest -c peat

# Helm install
helm install peat-mesh deploy/helm/peat-mesh/ \
  --set image.pullPolicy=Never \
  --set formationSecret=$(openssl rand -base64 32)
```

Helm chart: `deploy/helm/peat-mesh/` (StatefulSet, headless service, RBAC for EndpointSlice)

## Key Documentation

| Path | What |
|------|------|
| `docs/deployment.md` | Full deploy guide + functional test script |
| `docs/kubernetes.md` | K8s discovery API, EndpointSlice patterns |
| `docs/adr/` | 10 ADRs (K8s arch, extraction, discovery, transport, encryption, certs, adapters, Zarf, SDK, DHT) |

## Tests

Integration tests in `tests/`:
- `basic_e2e.rs` — Connectivity and peer discovery
- `dual_active_transport_e2e.rs` — Multi-transport concurrent sync
- `hierarchy_e2e.rs` — Topology formation and leader election
- `partition_integration_test.rs` — Partition detection and recovery
- `topology_manager_e2e.rs` — Topology state machine lifecycle
- `gc_tombstone_integration.rs` — Garbage collection with tombstones

## CI

- **GitHub Actions**: `ci.yml` (fmt, clippy, tests, feature builds), `release.yml` (crates.io publish)
- **Radicle/Goa**: `.goa` script — format, clippy, tests, feature builds; auto-accept/reject patches

## Related Repositories

- **peat** (`../hive/`): Main workspace — depends on `peat-mesh = "0.3.1"` from crates.io
- **peat-lite** (`../hive-lite/`): Embedded wire protocol — used by `lite-bridge` feature
- **peat-btle** (`../hive-btle/`): BLE transport — used by `bluetooth` feature
