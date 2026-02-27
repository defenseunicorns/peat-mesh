# Kubernetes Support

peat-mesh can run inside Kubernetes clusters using the `kubernetes` feature flag. This guide covers the code-level APIs added to support K8s deployments. For the deployment guide (binary, Docker, Helm, k3d), see [deployment.md](deployment.md). For architectural context, see [ADR-0001](adr/0001-kubernetes-istio-deployment.md).

## Feature flags

### Library usage

```toml
[dependencies]
peat-mesh = { version = "0.1.0", features = ["kubernetes"] }

# Binary crates must also enable a k8s-openapi version feature:
k8s-openapi = { version = "0.24", features = ["v1_32"] }
```

peat-mesh declares `k8s-openapi` as a dependency **without** a version feature (following the [k8s-openapi library crate convention](https://docs.rs/k8s-openapi/latest/k8s_openapi/#library-crates)). The final binary crate must enable one `v1_*` feature to select the target Kubernetes API version.

For non-test builds like `cargo check` or `cargo clippy`, set the environment variable instead:

```bash
K8S_OPENAPI_ENABLED_VERSION=1.32 cargo check --features kubernetes
```

### Binary target (`peat-mesh-node`)

The `node` meta-feature bundles everything needed for the Kubernetes binary:

```toml
# node = automerge-backend + broker + kubernetes + k8s-openapi/v1_32 + tracing-subscriber
cargo build --release --bin peat-mesh-node --features node
```

This is the recommended way to build for deployment. The `node` feature solves the k8s-openapi version selection automatically (selects `v1_32`), so you don't need to manage it separately.

See the [Deployment Guide](deployment.md) for Docker image building, Helm chart installation, and k3d cluster setup.

## Deterministic keypair derivation

Kubernetes pods are ephemeral. To maintain stable node identity without persistent storage, derive a `DeviceKeypair` deterministically from a seed (e.g., a K8s Secret) and a per-pod context string (e.g., pod name or StatefulSet ordinal):

```rust
use peat_mesh::security::DeviceKeypair;

// Same seed + context always produces the same keypair
let keypair = DeviceKeypair::from_seed(
    b"formation-shared-secret-from-k8s-secret",
    "peat-mesh-0",  // e.g., StatefulSet pod ordinal
)?;
```

Or via the builder:

```rust
use peat_mesh::{PeatMeshBuilder, MeshConfig};

let seed = std::env::var("PEAT_FORMATION_SECRET")
    .expect("PEAT_FORMATION_SECRET must be set")
    .into_bytes();
let pod_name = std::env::var("HOSTNAME").unwrap_or_else(|_| "default".into());

let mesh = PeatMeshBuilder::new(MeshConfig::default())
    .with_device_keypair_from_seed(&seed, &pod_name)?
    .build();
```

This uses HKDF-SHA256 internally. Different contexts produce different keys, so each pod in a StatefulSet gets a unique but reproducible identity.

## Kubernetes discovery

`KubernetesDiscovery` implements the `DiscoveryStrategy` trait by watching `EndpointSlice` resources in a Kubernetes namespace. It emits the standard `PeerFound`, `PeerLost`, and `PeerUpdated` events as pods scale up/down.

### Configuration

```rust
use peat_mesh::discovery::KubernetesDiscoveryConfig;
use std::time::Duration;

let config = KubernetesDiscoveryConfig {
    // Namespace to watch. None = read from service account mount or "default"
    namespace: Some("peat".to_string()),
    // Label selector for EndpointSlice resources
    label_selector: "app=peat-mesh".to_string(),
    // Annotation prefix for extracting peer metadata
    annotation_prefix: "peat.".to_string(),
    // Interval between re-list operations
    poll_interval: Duration::from_secs(30),
};
```

### Usage

```rust
use peat_mesh::discovery::{KubernetesDiscovery, KubernetesDiscoveryConfig};

let mut discovery = KubernetesDiscovery::new(KubernetesDiscoveryConfig::default());

// Get the event stream (can only be called once)
let mut events = discovery.event_stream()?;

// Start watching EndpointSlices
discovery.start().await?;

// Process discovery events
while let Some(event) = events.recv().await {
    match event {
        peat_mesh::discovery::DiscoveryEvent::PeerFound(peer) => {
            println!("Found peer: {} at {:?}", peer.node_id, peer.addresses);
        }
        peat_mesh::discovery::DiscoveryEvent::PeerLost(id) => {
            println!("Lost peer: {}", id);
        }
        peat_mesh::discovery::DiscoveryEvent::PeerUpdated(peer) => {
            println!("Updated peer: {}", peer.node_id);
        }
    }
}
```

### RBAC requirements

The pod's service account needs permission to list and watch EndpointSlices:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: peat-mesh-discovery
  namespace: peat
rules:
- apiGroups: ["discovery.k8s.io"]
  resources: ["endpointslices"]
  verbs: ["list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: peat-mesh-discovery
  namespace: peat
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: peat-mesh-discovery
subjects:
- kind: ServiceAccount
  name: peat-mesh
  namespace: peat
```

### Pod annotations

Peer metadata is extracted from EndpointSlice annotations with the configured prefix. For example, with the default `peat.` prefix:

```yaml
metadata:
  annotations:
    peat.formation: "alpha-company"
    peat.relay_url: "https://relay.example.com"
```

The `relay_url` annotation is special-cased: it populates `PeerInfo.relay_url` rather than appearing in the metadata map.

## Iroh configuration

`IrohConfig` provides bind address and relay URL configuration for the Iroh networking layer:

```rust
use peat_mesh::IrohConfig;

let iroh_config = IrohConfig {
    // Fixed bind address (instead of ephemeral port)
    bind_addr: Some("0.0.0.0:11204".parse().unwrap()),
    // Relay servers for NAT traversal
    relay_urls: vec!["https://relay.example.com".to_string()],
};
```

Wire it into `MeshConfig`:

```rust
use peat_mesh::{MeshConfig, IrohConfig};

let config = MeshConfig {
    iroh: IrohConfig {
        bind_addr: Some("0.0.0.0:11204".parse().unwrap()),
        ..Default::default()
    },
    ..Default::default()
};
```

`NetworkedIrohBlobStore::from_config()` applies both fields when constructing the iroh endpoint. When `relay_urls` is non-empty, a custom `RelayMap` is built and the endpoint uses `RelayMode::Custom`. When empty (the default), Iroh's default relay infrastructure is used.

## Readiness probe

The `broker` feature includes a `/api/v1/ready` endpoint for Kubernetes readiness probes:

```yaml
readinessProbe:
  httpGet:
    path: /api/v1/ready
    port: 8081
  initialDelaySeconds: 5
  periodSeconds: 10
```

The endpoint returns:
- **200 OK** with `{"ready": true, ...}` when the node is ready
- **503 Service Unavailable** with `{"ready": false, ...}` when not ready

The default `MeshBrokerState` implementation always returns ready. Override `readiness()` to add custom checks:

```rust
fn readiness(&self) -> ReadinessResponse {
    let transport_ready = self.transport.is_some();
    ReadinessResponse {
        ready: transport_ready,
        node_id: self.node_info().node_id,
        checks: vec![
            ReadinessCheck {
                name: "transport".into(),
                ready: transport_ready,
                message: if transport_ready { None } else { Some("no transport configured".into()) },
            },
        ],
    }
}
```

## Development

When developing peat-mesh with the `kubernetes` feature locally (outside a cluster), `KubernetesDiscovery::start()` will fail to create a client since there's no kubeconfig or service account. Use `with_client()` to inject a mock or test client:

```rust
let client = kube::Client::try_default().await?;
let discovery = KubernetesDiscovery::with_client(
    KubernetesDiscoveryConfig::default(),
    client,
);
```

For unit tests, the `extract_peers_from_endpoint_slice()` method is public and can be called directly with constructed `EndpointSlice` structs without a live cluster.
