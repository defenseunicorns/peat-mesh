# ADR-0001: Running eche-mesh Nodes in Kubernetes with Istio Service Mesh

**Status:** Accepted
**Date:** 2026-02-18
**Updated:** 2026-02-19
**Authors:** Kit Plummer
**Deciders:** Eche Core Team

## Context

eche-mesh was designed for tactical edge networks: air-gapped LANs, BLE mesh, and direct peer-to-peer connectivity. As Eche adoption grows, there is a need to run eche-mesh nodes inside Kubernetes clusters with Istio service mesh for cloud-hosted or hybrid deployments. This introduces significant architectural tensions between eche-mesh's peer-to-peer networking model and Kubernetes/Istio's proxy-mediated, service-oriented model.

This ADR examines the impacts across five domains: mesh creation, certificate management, discovery, Iroh/BLE connectivity, and cluster-to-cluster federation.

## Current Architecture Summary

| Component | Current Mechanism | Protocol | Network Assumptions |
|---|---|---|---|
| Discovery | mDNS (`_eche._udp.local.`) | UDP multicast 224.0.0.251:5353 | Same broadcast domain |
| Identity | Ed25519 keypairs + formation keys | N/A (application layer) | None |
| Iroh sync | QUIC (UDP) via iroh-blobs | UDP ephemeral ports | Direct IP reachability |
| Bypass channel | Custom UDP multicast | UDP 239.1.1.100:5150 | Multicast-capable network |
| BLE transport | BlueZ D-Bus / eche-btle | BLE GATT | Physical proximity |
| Broker API | HTTP/WebSocket (Axum) | TCP 8081 | Standard TCP |

## Decision Drivers

- Maintain eche-mesh's offline-first, zero-bootstrap design philosophy
- Support hybrid deployments where cloud nodes coexist with edge/BLE nodes
- Avoid creating hard dependencies on Kubernetes or Istio
- Preserve the existing security model while interoperating with Istio mTLS
- Minimize latency overhead for CRDT synchronization

---

## Impact Analysis

### 1. Mesh Creation

**Problem:** eche-mesh forms meshes via mDNS multicast discovery and direct peer connections. Kubernetes pod networks (Calico, Cilium, Flannel) do not support multicast. Istio's Envoy sidecar intercepts all traffic, adding a proxy hop to every connection.

**Specific challenges:**
- mDNS uses UDP multicast (`224.0.0.251`), which is dropped by all major CNI plugins
- Bypass channel uses custom multicast group `239.1.1.100`, also unsupported
- Pod IP addresses are ephemeral and change on restart
- Istio's Envoy sidecar intercepts outbound connections and applies traffic policies
- eche-mesh expects direct peer-to-peer UDP, not proxy-mediated TCP

**Options:**

| Option | Description | Pros | Cons |
|---|---|---|---|
| **A. Static discovery + headless Service** | Use `StaticDiscovery` with K8s headless Service DNS for stable peer addresses | Works with existing code; no mDNS needed | Must maintain peer list; doesn't auto-scale |
| **B. Custom K8s discovery provider** | New `KubernetesDiscovery` strategy using K8s API (watch Endpoints) | Auto-discovers pods; scales naturally | New code; K8s API dependency; RBAC needed |
| **C. Host networking** | `hostNetwork: true` to bypass pod network | mDNS works; bypass channel works | Breaks pod isolation; port conflicts; Istio sidecar complications |
| **D. Sidecar discovery agent** | Sidecar container that watches K8s Endpoints and feeds static config to eche-mesh | No eche-mesh code changes; K8s-native | Operational complexity; sidecar coordination |

**Recommendation:** Start with **Option A** (static discovery + headless Service) for initial deployment, then build **Option B** (KubernetesDiscovery) as a proper discovery strategy behind the existing `DiscoveryStrategy` trait. This follows the existing pluggable pattern.

### 2. Certificate and Identity Management

**Problem:** eche-mesh uses its own identity system (Ed25519 device keypairs + HMAC-SHA256 formation keys). Istio enforces its own mTLS using SPIFFE identities and X.509 certificates issued by `istiod`. These two identity systems will overlap, potentially conflict, or create redundant encryption layers.

**Specific challenges:**
- **Double encryption:** Istio mTLS encrypts pod-to-pod traffic. eche-mesh's optional ChaCha20-Poly1305 encryption adds a second layer. This is wasteful but not harmful.
- **Identity mismatch:** Istio identifies workloads by SPIFFE ID (`spiffe://cluster.local/ns/eche/sa/eche-node`). eche-mesh identifies nodes by Ed25519 public key (first 16 bytes). These are independent identity planes.
- **Formation keys:** Pre-shared formation keys must still be distributed to pods. K8s Secrets or an external secret manager (Vault) would be needed.
- **Key persistence:** `DeviceKeypair` is currently saved to disk. Pod restarts with ephemeral storage would generate new identities. StatefulSet with PVCs would preserve them.

**Options:**

| Option | Description | Pros | Cons |
|---|---|---|---|
| **A. Dual identity (recommended)** | Keep both systems independent. Istio handles transport security. eche-mesh handles application identity. | Clean separation; no coupling; works if Istio is removed | Double encryption overhead (negligible for CRDT docs) |
| **B. Istio-only identity** | Replace eche-mesh crypto with Istio mTLS; derive node IDs from SPIFFE | Single identity plane | Hard dependency on Istio; breaks edge/BLE; major refactor |
| **C. Bridge identities** | Map SPIFFE IDs to eche-mesh node IDs via a registration service | Unified identity view | Complex; new service to maintain; partial coupling |

**Recommendation:** **Option A** (dual identity). Istio provides transport-layer security between pods. eche-mesh provides application-layer identity that works identically on edge, BLE, and cloud. Formation keys should be injected via K8s Secrets mounted as volumes.

**Key persistence strategy:**
- Deploy as **StatefulSet** with PersistentVolumeClaims so `DeviceKeypair` survives pod restarts
- Alternatively, derive keypairs deterministically from a stable seed (e.g., pod ordinal + formation secret) so identity is reproducible without persistent storage

### 3. Discovery

**Problem:** eche-mesh's discovery layer assumes LAN-level visibility. In Kubernetes, nodes need to discover peers across the pod network without multicast. Cross-cluster discovery adds another dimension.

**Specific challenges:**
- mDNS is non-functional in pod networks
- K8s Services provide stable DNS but eche-mesh expects `DiscoveryEvent` streams
- Istio's service registry is separate from eche-mesh's peer registry
- Pods scale dynamically; peer lists must track additions and removals
- Cross-namespace or cross-cluster peers need different discovery mechanisms

**Proposed `KubernetesDiscovery` strategy:**

```
KubernetesDiscovery
├── Watches: Endpoints/EndpointSlice API for labeled pods
├── Emits: PeerFound / PeerLost / PeerUpdated events
├── Labels: app=eche-mesh, eche.formation=<name>
├── Annotations: eche.node-id, eche.public-key, eche.iroh-endpoint
└── Scope: namespace (default) or cluster-wide (configurable)
```

**Integration with existing trait:**

```rust
// KubernetesDiscovery implements the existing DiscoveryStrategy trait
// No changes to EcheMesh or other discovery strategies needed
impl DiscoveryStrategy for KubernetesDiscovery {
    async fn start(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    fn events(&self) -> broadcast::Receiver<DiscoveryEvent>;
}
```

**Cross-cluster discovery** is addressed in Section 6 below.

### 4. Iroh Connections (QUIC/UDP)

**Problem:** Iroh uses QUIC (UDP-based) for blob synchronization. Istio's Envoy sidecar is primarily designed for TCP (HTTP/1.1, HTTP/2, gRPC). UDP handling in Istio is limited and adds complexity.

**Specific challenges:**
- **Envoy and UDP:** Istio's Envoy proxy does not natively proxy arbitrary UDP. QUIC support in Envoy exists but is focused on HTTP/3, not custom ALPN protocols like `iroh-blobs`.
- **Port management:** Iroh uses ephemeral UDP ports by default. K8s requires declared container ports for service exposure.
- **NAT traversal:** Iroh's hole-punching assumes direct internet access. Pods behind cluster NAT cannot hole-punch to external peers.
- **Relay servers:** Iroh supports relay servers for NAT traversal, but eche-mesh doesn't currently configure them.

**Options:**

| Option | Description | Pros | Cons |
|---|---|---|---|
| **A. Bypass Envoy for QUIC** | Annotate pods to exclude Iroh's UDP port from Istio interception (`traffic.sidecar.istio.io/excludeOutboundPorts`) | Direct QUIC; lowest latency; simplest | Loses Istio observability/policy for Iroh traffic |
| **B. Fixed Iroh port + UDP passthrough** | Configure Iroh with a fixed port; use K8s Service (UDP) | Predictable networking; K8s-native | Still bypasses Envoy; need to manage port assignment |
| **C. Iroh relay for external peers** | Deploy an Iroh relay server at the cluster edge; internal pods use direct QUIC | External peers can reach cluster nodes; clean boundary | New infrastructure component; relay adds latency |
| **D. Fallback TCP transport** | Add a TCP-based sync transport for in-cluster use (Iroh supports custom transports) | Works fully within Istio; full mTLS coverage | New transport implementation; different perf characteristics |

**Recommendation:** **Option B** (fixed port + UDP passthrough) for intra-cluster traffic, combined with **Option C** (relay server) for cluster-to-external connectivity.

**Concrete steps:**
1. Add a `bind_port` configuration option to `NetworkedIrohBlobStore` (currently ephemeral)
2. Exclude Iroh's UDP port from Envoy interception via pod annotations
3. Deploy Iroh relay server(s) as a cluster-edge service for external peer connectivity
4. Advertise relay URLs in `KubernetesDiscovery` pod annotations

### 5. BLE Connections Outside the Cluster

**Problem:** BLE is inherently a physical-proximity protocol. Kubernetes nodes (cloud VMs) don't have BLE radios. Edge devices with BLE need a bridge to reach cluster-hosted nodes.

**Architecture:**

```
                    ┌─────────────────────────────────┐
                    │        Kubernetes Cluster        │
                    │                                  │
                    │  ┌──────────┐    ┌──────────┐   │
                    │  │ eche-mesh│◄──►│ eche-mesh│   │
                    │  │  pod A   │    │  pod B   │   │
                    │  └────▲─────┘    └──────────┘   │
                    │       │ QUIC/Iroh               │
                    │  ┌────┴─────┐                    │
                    │  │  Gateway  │ (NodePort/LB)     │
                    └──┤  Service  ├───────────────────┘
                       └────▲─────┘
                            │ QUIC (via relay or direct)
                    ┌───────┴────────┐
                    │  Edge Gateway   │
                    │  (Pi / laptop)  │
                    │  eche-mesh +    │
                    │  BLE transport  │
                    └───────▲────────┘
                            │ BLE GATT
              ┌─────────────┴──────────────┐
              │                            │
        ┌─────┴─────┐              ┌──────┴────┐
        │ BLE device │              │ BLE device │
        │ (ATAK EUD) │              │ (sensor)   │
        └───────────┘              └───────────┘
```

**Key design decisions:**
- **Edge gateway pattern:** A eche-mesh node running on hardware with a BLE radio acts as a bridge between BLE devices and the cluster
- **No BLE in-cluster:** Do not attempt to passthrough BLE to containers; it's the wrong abstraction
- **Gateway is a full mesh peer:** The edge gateway participates in CRDT sync over both BLE (to local devices) and QUIC/Iroh (to cluster)
- **Multi-homed identity:** The gateway node has a single `DeviceKeypair` but participates in both transport domains

**Impact on eche-mesh:**
- No code changes needed for BLE; the transport abstraction already handles this
- Edge gateway is simply a eche-mesh node with both `bluetooth` and `automerge-backend` features enabled
- Need to ensure the gateway can maintain simultaneous BLE and QUIC connections (already supported by `TransportManager`)

### 6. Cluster-to-Cluster Federation

**Problem:** Multiple Kubernetes clusters (e.g., regional deployments, classification boundaries) need to synchronize eche-mesh state while maintaining independent mesh control planes.

**Options:**

| Option | Description | Pros | Cons |
|---|---|---|---|
| **A. Iroh relay bridge** | Dedicated relay servers at cluster edges; peers connect via relay URLs | Uses existing Iroh infrastructure; simple | Relay is a bottleneck; single point of failure |
| **B. Gateway peer pattern** | Designated "federation gateway" pods in each cluster connect to each other | Controlled data flow; can apply policies | Must manage gateway peer list; new operational role |
| **C. Istio multi-cluster** | Use Istio's built-in multi-cluster mesh (east-west gateway) | Transparent to eche-mesh; Istio-native | Complex Istio configuration; requires trust domain setup |
| **D. VPN underlay** | WireGuard or similar VPN between clusters; eche-mesh sees a flat network | Works for all transports; simple for eche-mesh | VPN management overhead; latency; not always permitted |

**Recommendation:** **Option B** (gateway peer pattern) as the primary mechanism, with **Option C** (Istio multi-cluster) as an alternative for organizations already running multi-cluster Istio.

**Federation gateway design:**
- Specific pods labeled `eche.role=federation-gateway`
- Configured with static peer addresses of remote cluster gateways
- Participates in CRDT sync and propagates documents across cluster boundary
- Can apply filtering/policy on what documents cross the boundary (e.g., classification level)

---

## Deployment Architecture (Proposed)

```yaml
# Conceptual - not production-ready
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: eche-mesh
  namespace: eche
spec:
  serviceName: eche-mesh
  replicas: 3
  template:
    metadata:
      labels:
        app: eche-mesh
      annotations:
        # Exclude Iroh QUIC port from Envoy interception
        traffic.sidecar.istio.io/excludeOutboundPorts: "11204"
        traffic.sidecar.istio.io/excludeInboundPorts: "11204"
        # eche-mesh identity metadata
        eche.node-id: ""       # populated by init container
        eche.public-key: ""    # populated by init container
    spec:
      containers:
      - name: eche-mesh
        ports:
        - name: broker-http
          containerPort: 8081
          protocol: TCP
        - name: iroh-quic
          containerPort: 11204
          protocol: UDP
        volumeMounts:
        - name: identity
          mountPath: /data/identity
        - name: formation-keys
          mountPath: /etc/eche/formation-keys
          readOnly: true
        env:
        - name: ECHE_DISCOVERY
          value: "kubernetes"
        - name: ECHE_IROH_PORT
          value: "11204"
  volumeClaimTemplates:
  - metadata:
      name: identity
    spec:
      accessModes: ["ReadWriteOnce"]
      resources:
        requests:
          storage: 100Mi
---
apiVersion: v1
kind: Service
metadata:
  name: eche-mesh
  namespace: eche
spec:
  clusterIP: None  # Headless for peer discovery
  selector:
    app: eche-mesh
  ports:
  - name: broker-http
    port: 8081
    protocol: TCP
  - name: iroh-quic
    port: 11204
    protocol: UDP
```

## 7. Exposing the Mesh to In-Cluster Services

**Problem:** Other workloads running in the same Kubernetes cluster need to consume mesh state (read documents, subscribe to updates, write commands). These services may be written in Go, Python, TypeScript, or other languages and cannot embed the Rust eche-mesh library directly.

**Two integration paths exist (see ADR-0007, ADR-0009):**

### Path A: Consumer Interface Adapters (ADR-0007)

The eche-mesh broker module (feature-gated behind `broker`) already provides HTTP/REST and WebSocket endpoints. In K8s, this becomes the primary way for cluster services to interact with mesh state.

```
┌──────────────────────────────────────────────────────────────┐
│                    Kubernetes Cluster                          │
│                                                               │
│  ┌──────────┐     ┌──────────────────────────────┐           │
│  │ Go       │────►│ eche-mesh pod                 │           │
│  │ Operator │ HTTP│  ┌────────────────────────┐   │           │
│  └──────────┘  :  │  │ Broker (Axum)          │   │           │
│                8081│  │  GET /api/v1/documents │   │           │
│  ┌──────────┐     │  │  POST /api/v1/command  │   │           │
│  │ Web      │────►│  │  WS /ws (subscribe)    │   │           │
│  │Dashboard │ WS  │  └────────────────────────┘   │           │
│  └──────────┘     │            │                   │           │
│                   │            ▼                   │           │
│  ┌──────────┐     │  ┌────────────────────────┐   │           │
│  │ Python   │────►│  │ CRDT Store (Automerge) │   │           │
│  │ Script   │ HTTP│  └────────────────────────┘   │           │
│  └──────────┘     └──────────────────────────────┘           │
│                                                               │
└──────────────────────────────────────────────────────────────┘
```

**K8s-specific considerations for the broker:**
- Expose broker via ClusterIP Service (TCP 8081) for internal access
- Istio sidecar handles mTLS for broker traffic automatically (it's standard HTTP/WS)
- Use Istio `AuthorizationPolicy` to restrict which services can reach the broker
- SSE endpoint (`GET /api/v1/stream`) for lightweight event subscriptions
- Health/readiness probes via `GET /api/v1/status`

### Path B: SDK Integration (ADR-0009)

For services that can embed eche-mesh directly (Go via cgo, Python via PyO3), the SDK path provides full CRDT participation with no adapter overhead.

**K8s operator example (Go):**
```go
// A Kubernetes operator that is also a eche-mesh peer
node, _ := eche.NewEcheNode(eche.Config{
    PlatformID:  os.Getenv("HOSTNAME"),
    MeshBackend: "automerge",
    Transports:  []string{"iroh"},
})
node.Start(ctx)

// Full CRDT sync - this operator IS a mesh node
platforms, _ := node.SubscribePlatforms(ctx)
for p := range platforms {
    // Reconcile Kubernetes resources based on mesh state
}
```

**Trade-offs in K8s context:**

| Aspect | Broker/Adapter (Path A) | SDK (Path B) |
|---|---|---|
| Languages | Any (HTTP/WS) | Go, Python, Rust (FFI) |
| Latency | +50-200ms | Sync only (~10-50ms) |
| Offline resilience | None (needs adapter pod) | Full (local CRDT replica) |
| Istio integration | Full (HTTP traffic) | Partial (QUIC bypasses Envoy) |
| Operational complexity | Low (standard HTTP) | Medium (each service is a mesh peer) |
| Scaling | Single broker serves many clients | Each pod joins mesh independently |

**Recommendation:** Use **Path A** (broker) as the default for most in-cluster consumers. Reserve **Path B** (SDK) for services that need offline resilience, low latency, or full mesh participation (e.g., federation gateways, Zarf operators).

### Zarf/UDS Integration (ADR-0008)

For Zarf-based deployments using UDS Core (which includes Istio), eche-mesh serves as the metadata backplane for package distribution and deployment coordination across the hierarchy. The Zarf integration consumes eche-mesh via either the broker API or Go SDK to:

- Advertise available packages across the mesh
- Distribute deployment intents to target nodes
- Aggregate deployment status up the hierarchy
- Coordinate with Pepr controllers for K8s-native automation

---

## Required Code Changes

### eche-mesh crate

| Change | Priority | Effort | Description |
|---|---|---|---|
| `KubernetesDiscovery` strategy | High | Medium | New discovery module implementing `DiscoveryStrategy` using K8s API |
| Fixed Iroh bind port | High | Low | Configuration option for `NetworkedIrohBlobStore` bind port |
| Iroh relay support | Medium | Medium | Configure relay server URLs for NAT traversal |
| Feature flag `kubernetes` | High | Low | Gate K8s-specific dependencies (`kube-rs`) behind feature |
| Deterministic keypair derivation | Low | Low | Optional: derive `DeviceKeypair` from seed for stateless pods |
| Health/readiness probes | Medium | Low | HTTP endpoints for K8s liveness/readiness checks |

### Infrastructure / Ops

| Change | Priority | Effort | Description |
|---|---|---|---|
| Dockerfile | High | Low | Multi-stage build for eche-mesh binary |
| Helm chart | High | Medium | Parameterized K8s manifests |
| Iroh relay deployment | Medium | Medium | Relay server at cluster edge |
| CI/CD pipeline | Medium | Medium | Build + push container images |
| RBAC manifests | High | Low | ServiceAccount + Role for K8s API access (Endpoints watch) |

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| QUIC blocked by network policy | Medium | High | Document required UDP ports; provide NetworkPolicy manifests |
| Istio upgrade breaks UDP bypass | Low | Medium | Pin Istio version; integration tests in CI |
| Pod identity churn on scaling | Medium | Medium | StatefulSet + PVC; deterministic key derivation |
| Latency regression vs. direct mesh | High | Low | Benchmark; acceptable for CRDT (eventual consistency) |
| BLE gateway single point of failure | Medium | High | Deploy multiple gateways; BLE devices connect to nearest |

## Decision

**Accepted.** Key decisions resolved:

1. **KubernetesDiscovery lives in eche-mesh** behind the `kubernetes` feature flag (implemented).
2. **Iroh relay**: configurable via `IrohConfig.relay_urls`; self-hosted or public, operator's choice.
3. **Federation**: Gateway peer pattern first (Option B). Istio multi-cluster as a future option.
4. **MVP scope**: Phase 1 (library APIs) and Phase 2 (deployment infrastructure) are complete. See implementation status below.

## Implementation Status

### Phase 1: Library APIs (complete — commit `3e34890`)

| Component | Status | Location |
|-----------|--------|----------|
| `DeviceKeypair::from_seed()` | Done | `src/security/keypair.rs` |
| `KubernetesDiscovery` | Done | `src/discovery/kubernetes.rs` |
| `KubernetesDiscoveryConfig` | Done | `src/discovery/kubernetes.rs` |
| `IrohConfig` (bind_addr, relay_urls) | Done | `src/config.rs` |
| Readiness probe (`/api/v1/ready`) | Done | `src/broker/routes.rs` |
| `kubernetes` feature flag | Done | `Cargo.toml` |

### Phase 2: Deployment Infrastructure (complete)

| Component | Status | Location |
|-----------|--------|----------|
| `eche-mesh-node` binary | Done | `src/bin/eche-mesh-node.rs` |
| `node` meta-feature | Done | `Cargo.toml` |
| Dockerfile (multi-stage) | Done | `deploy/Dockerfile` |
| Helm chart (StatefulSet, headless Service, RBAC) | Done | `deploy/helm/eche-mesh/` |
| Deployment guide | Done | `docs/deployment.md` |

### Verified

- 3-replica StatefulSet on k3d cluster: all pods Running + Ready
- Health probe (`/api/v1/health`): 200 OK
- Readiness probe (`/api/v1/ready`): 200 OK
- EndpointSlice created with all pod IPs
- Deterministic keypair survives pod restart (same `device_id`)
- Distinct `device_id` per pod (different `HOSTNAME` context)

### Remaining work

- Wire `KubernetesDiscovery` events into active Iroh transport connections
- Iroh relay server deployment manifests
- Cross-cluster federation gateway (static peer + hybrid discovery)
- Istio sidecar annotations for UDP passthrough
- CI/CD pipeline for container image builds

## References

### Related ADRs

- [ADR-0002](0002-eche-mesh-extraction.md) - eche-mesh Extraction (architecture and API surface)
- [ADR-0003](0003-peer-discovery-architecture.md) - Peer Discovery Architecture (beacon + DHT tiers)
- [ADR-0004](0004-pluggable-transport-abstraction.md) - Pluggable Transport Abstraction (PACE, multi-transport)
- [ADR-0005](0005-e2e-encryption-key-management.md) - E2E Encryption and Key Management
- [ADR-0006](0006-membership-certificates-tactical-trust.md) - Membership Certificates and Tactical Trust
- [ADR-0007](0007-consumer-interface-adapters.md) - Consumer Interface Adapters (broker HTTP/WS/TCP)
- [ADR-0008](0008-zarf-uds-integration.md) - Zarf/UDS Integration for Tactical Software Delivery
- [ADR-0009](0009-sdk-integration.md) - Eche SDK Integration (Go, Python, Kotlin bindings)

### External

- [Istio Traffic Management - Protocol Selection](https://istio.io/latest/docs/ops/configuration/traffic-management/protocol-selection/)
- [Iroh Relay Servers](https://www.iroh.computer/docs/concepts/relay)
- [Kubernetes Headless Services](https://kubernetes.io/docs/concepts/services-networking/service/#headless-services)
- [SPIFFE Identity Framework](https://spiffe.io/)
- [Envoy QUIC Support](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/http/http3)
- [Zarf Documentation](https://docs.zarf.dev/)
- [UDS Core](https://github.com/defenseunicorns/uds-core)
- [Defense Unicorns](https://github.com/defenseunicorns)
