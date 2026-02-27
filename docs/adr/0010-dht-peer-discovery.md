# ADR-0010: DHT Peer Discovery via Iroh/Pkarr

**Status**: Proposed
**Date**: 2026-02-22
**Deciders**: Kit Plummer, Claude Code
**Related**: ADR-0003 (Peer Discovery Architecture), Radicle issue `fef01a8`

## Context

ADR-0003 established a multi-tier discovery architecture: Tier 1 (beacon-based local
discovery) is implemented, and Tier 2 (DHT-based global discovery) was reserved for
future work. The original ADR proposed libp2p-kad (Kademlia) for DHT discovery.

Since then, peat-mesh has standardized on **Iroh 0.95** as the networking layer. Iroh
ships with built-in discovery primitives that use the **Mainline BitTorrent DHT** via
**pkarr** (Public Key Addressable Resource Records). This ADR evaluates Iroh's
discovery APIs and proposes an implementation path that leverages them instead of
adding libp2p-kad as a separate dependency.

### Current State

peat-mesh uses `StaticProvider` as the sole Iroh discovery mechanism. External
discovery strategies (Kubernetes EndpointSlice, mDNS, static TOML) feed peer addresses
into the `StaticProvider` via `PeerConnector`:

```
DiscoveryStrategy (K8s/mDNS/static)
  -> DiscoveryEvent (PeerFound/Lost/Updated)
    -> PeerConnector
      -> StaticProvider.add_endpoint_info(addr)
        -> Iroh Endpoint can now dial that peer
```

The `discovery-local-network` feature is enabled in `Cargo.toml` but **not wired**
into the endpoint builder. Pkarr is pulled in transitively but **not used**.

### Problem Statement

1. **No wide-area discovery**: Nodes in different clusters/networks cannot find each
   other without pre-configured static addresses or shared infrastructure (K8s).
2. **No internet-scale reachability**: A node behind NAT has no way to advertise its
   relay URL and address info to nodes that don't already know about it.
3. **Formation bootstrap**: New nodes joining a formation need a way to discover
   existing members without a central directory.

## Iroh 0.95 Discovery Primitives

### Discovery Trait

```rust
pub trait Discovery: Send + Sync + Debug {
    fn publish(&self, info: &EndpointAddr) { }          // optional
    fn resolve(&self, id: EndpointId) -> BoxStream<DiscoveryItem> { empty() }
}
```

- `publish()` is called by the endpoint to advertise its own addresses
- `resolve()` is called when dialing a peer whose address is unknown
- Multiple backends combine via `ConcurrentDiscovery`

### Available Backends

| Backend | Mechanism | Scope | Key |
|---------|-----------|-------|-----|
| `StaticProvider` | In-memory address map | Process-local | EndpointId |
| `PkarrPublisher` | HTTP PUT to pkarr relay | Global | Node public key |
| `PkarrResolver` | HTTP GET from pkarr relay | Global | Node public key |
| `DnsDiscovery` | DNS TXT record lookup | Global | Node public key |
| `MdnsDiscovery` | Local network broadcast | LAN | EndpointId |
| `ConcurrentDiscovery` | Combines N backends | Mixed | - |

### Pkarr and BEP44

Pkarr publishes signed DNS resource records to the **Mainline BitTorrent DHT**
(BEP44 mutable items). Each record is:

- **Keyed by**: Ed25519 public key of the signer
- **Signed by**: Corresponding Ed25519 private key
- **Content**: DNS resource records (A, AAAA, TXT, SVCB, etc.)
- **TTL**: DHT drops records after ~2 hours; periodic republishing required
- **Size limit**: ~1000 bytes (BEP44 v field limit)

Iroh uses pkarr to publish endpoint addressing info: direct addresses, relay URL,
and (optionally) a `UserData` string (max 245 bytes, UTF-8).

### UserData

```rust
pub fn user_data_for_discovery(self, user_data: UserData) -> Builder
```

Applications can attach up to 245 bytes of opaque metadata to their pkarr record.
Iroh does not interpret `UserData`; it is propagated to resolvers alongside address
info. Peat can use this to carry formation identifiers.

### iroh-gossip

Separate from the Discovery trait, `iroh-gossip` provides topic-based pub/sub:

- Swarms organized by **TopicId** (32-byte identifier)
- HyParView membership + PlumTree broadcast
- Requires at least one bootstrap peer to join a topic
- Maintains ~5 peer connections per topic

## The Formation-vs-Node Mismatch

Iroh's discovery is **node-keyed**: you resolve a specific `EndpointId` (public key)
to get its addresses. Peat needs **formation-keyed** discovery: "find all peers in
formation X."

This is a fundamental mismatch:

- **Pkarr resolve**: "I know node `abc123`'s public key, give me its addresses" (works)
- **Peat needs**: "I have formation secret `S`, give me all member nodes" (not directly supported)

You cannot query the DHT for "all records with UserData containing formation ID F"
because pkarr records are keyed by public key, not by content.

## Decision

We adopt a **three-layer discovery architecture** that leverages Iroh's existing
primitives without requiring libp2p-kad:

### Layer 1: Node Reachability (Pkarr)

Every peat-mesh node publishes its own addressing info to the DHT via
`PkarrPublisher`. This makes any node reachable by anyone who knows its
`EndpointId`:

```rust
let builder = Endpoint::builder()
    .discovery(ConcurrentDiscovery::from_services(vec![
        Box::new(static_provider.clone()),        // local/manual
        Box::new(PkarrPublisher::default()),       // publish to DHT
        Box::new(PkarrResolver::default()),        // resolve from DHT
    ]))
    .user_data_for_discovery(
        UserData::try_from(format!("peat:f={}", formation_id))?
    );
```

**What this solves**: Once you know a peer's `EndpointId`, you can dial it from
anywhere on the internet without pre-configuring addresses. NAT traversal works
via Iroh relays, and the DHT record points to the current relay URL.

**What this doesn't solve**: Finding formation members in the first place.

### Layer 2: Formation Membership (Gossip Rendezvous)

Use `iroh-gossip` with a deterministic `TopicId` derived from the formation secret
to bootstrap formation membership:

```rust
// Deterministic topic from formation secret
let topic_id = {
    let hk = Hkdf::<Sha256>::new(None, &formation_secret);
    let mut topic = [0u8; 32];
    hk.expand(b"peat-mesh:gossip-topic", &mut topic)?;
    TopicId::from(topic)
};

// Join the gossip topic
// Bootstrap peers come from Layer 1 (known EndpointIds) or Layer 3
let gossip = Gossip::builder().spawn(endpoint.clone()).await?;
let topic = gossip.join(topic_id, bootstrap_peers).await?;
```

Formation members periodically broadcast their `EndpointId` on the gossip topic.
New members joining the topic receive the full peer list via gossip protocol
(HyParView + PlumTree ensures logarithmic dissemination).

**What this solves**: Formation-level peer discovery. Any member can find all
other members by joining the gossip topic.

**What this requires**: At least one bootstrap peer's `EndpointId` to join the
gossip swarm. This comes from Layer 3.

### Layer 3: Bootstrap (Existing Strategies + Seed Peers)

Bootstrap peers for gossip come from the existing discovery infrastructure:

1. **Kubernetes discovery**: K8s EndpointSlice provides initial peers in-cluster
2. **mDNS**: Local network peers found automatically
3. **Static config**: Pre-configured seed peers (TOML or env var)
4. **Well-known seed**: Formation deployer publishes a "seed node" EndpointId
   that new members use for initial gossip bootstrap

```rust
enum BootstrapSource {
    Kubernetes(KubernetesDiscovery),
    Mdns(MdnsDiscovery),
    Static(Vec<EndpointId>),
    SeedNode(EndpointId),  // well-known formation seed
}
```

### Combined Flow

```
New node starts with formation_secret + optional seed EndpointId
  |
  v
Layer 1: Publish own pkarr record (now globally reachable)
  |
  v
Layer 3: Find bootstrap peer via K8s/mDNS/static/seed
  |
  v
Layer 2: Join gossip topic (derived from formation_secret)
  |
  v
Gossip receives EndpointIds of all formation members
  |
  v
Layer 1: Resolve each member's address via pkarr DHT
  |
  v
PeerConnector feeds addresses into StaticProvider
  -> Iroh can now dial any formation member
```

## Implementation Plan

### Phase 1: Pkarr Node Reachability

**Scope**: Enable DHT publishing/resolving for all nodes.

1. Add `PkarrPublisher` + `PkarrResolver` to `ConcurrentDiscovery` in
   `NetworkedIrohBlobStore::build_endpoint()`
2. Add `UserData` with formation identifier to endpoint builder
3. Add `IrohConfig` fields: `enable_dht: bool`, `pkarr_relay_url: Option<String>`
4. Feature-gate behind `dht-discovery` (depends on `automerge-backend`)

**Files**:
- `src/storage/iroh_blob_store.rs` — endpoint builder changes
- `src/config.rs` — new config fields
- `Cargo.toml` — feature flag

### Phase 2: Gossip Formation Discovery

**Scope**: Formation-level peer discovery via gossip.

1. Add `iroh-gossip` dependency (optional, behind `dht-discovery` feature)
2. Create `src/discovery/gossip.rs` implementing `DiscoveryStrategy`
3. Derive `TopicId` from formation secret via HKDF
4. Emit `DiscoveryEvent::PeerFound` when gossip receives new member EndpointIds
5. Periodically broadcast own EndpointId on gossip topic

**Files**:
- `src/discovery/gossip.rs` (new)
- `src/discovery/mod.rs` — register gossip strategy
- `Cargo.toml` — iroh-gossip dep

### Phase 3: Bootstrap Integration

**Scope**: Wire gossip bootstrap to existing discovery strategies.

1. `HybridDiscovery` feeds initial bootstrap peers to gossip
2. Add `PEAT_SEED_PEERS` env var (comma-separated EndpointIds)
3. Update `peat-mesh-node` binary to wire gossip when `dht-discovery` enabled
4. Add K8s annotation for EndpointId (already partially implemented)

**Files**:
- `src/bin/peat-mesh-node.rs` — gossip wiring
- `src/discovery/hybrid.rs` — feed bootstrap to gossip
- `src/peer_connector.rs` — handle gossip-originated events

### Phase 4: MdnsDiscovery Activation

**Scope**: Enable Iroh's built-in mDNS (already feature-gated, just not wired).

1. Add `MdnsDiscovery` to `ConcurrentDiscovery` when
   `config.discovery.mdns_enabled` is true
2. This provides zero-config LAN discovery without our custom `mdns-sd` strategy

**Files**:
- `src/storage/iroh_blob_store.rs` — add MdnsDiscovery to builder

## Rationale

### Why Pkarr/Mainline DHT Instead of libp2p-kad?

1. **Already a dependency**: Iroh 0.95 ships pkarr; no new crate needed
2. **Battle-tested infrastructure**: Mainline DHT has millions of nodes
3. **Integrated with Iroh**: Works with `Discovery` trait, endpoint builder,
   and relay infrastructure out of the box
4. **NAT-friendly**: Pkarr records include relay URLs for NAT traversal

### Why Gossip for Formation Discovery Instead of a Shared DHT Key?

BEP44 mutable items are **single-writer**: only the keypair holder can update a
record. A "formation key" approach would require:
- All members sharing the formation's private key (security risk)
- Or a leader-based approach (single point of failure)

Gossip avoids these problems:
- Each member publishes their own info (multi-writer by design)
- HyParView provides resilient partial-mesh connectivity
- PlumTree ensures reliable broadcast with logarithmic overhead
- TopicId derived from formation secret restricts membership

### Why Not Gossip-Only?

Gossip requires bootstrap peers. Without pkarr, you need another mechanism to
find at least one peer's current address. Pkarr provides the "address book" that
makes gossip bootstrap work from anywhere on the internet.

## Consequences

### Positive

1. **Internet-scale discovery**: Nodes can find each other across NATs and networks
2. **No new heavyweight dependencies**: Uses Iroh's existing pkarr and gossip
3. **Formation-scoped**: Gossip topics are formation-specific (derived from secret)
4. **Backward-compatible**: Existing K8s/mDNS/static discovery continues to work
5. **Incremental adoption**: Each phase is independently valuable

### Negative

1. **DHT republishing overhead**: Nodes must republish pkarr records every ~1 hour
2. **Gossip connections**: Each node maintains ~5 connections per formation topic
3. **Bootstrap dependency**: Gossip still needs at least one known peer to start
4. **Feature complexity**: New `dht-discovery` feature flag and config options

### Neutral

1. **Migration from mdns-sd**: Phase 4 replaces our custom mDNS with Iroh's built-in
2. **PeerConnector changes**: Needs to handle gossip-originated events alongside
   existing discovery events

## Alternatives Considered

### 1. libp2p-kad (Original ADR-0003 Proposal)

**Rejected**: Would add libp2p as a parallel networking stack alongside Iroh.
Pkarr + Iroh discovery achieves the same goal without the dependency conflict.

### 2. Shared Formation Pkarr Key

**Rejected**: BEP44 is single-writer. Sharing the formation private key with all
members is a security risk, and leader-based approaches add single points of failure.

### 3. Pkarr UserData Scanning

**Not Feasible**: DHT does not support content-based queries. You cannot search
for "all records where UserData contains formation-id=X".

### 4. Relay-Based Rendezvous

**Deferred**: Iroh relay servers could potentially serve as rendezvous points
(all formation members connect to the same relay). This is simpler but creates
relay server dependency and doesn't work if relays are unavailable.

### 5. Direct DHT (mainline crate)

**Rejected**: Using the `mainline` crate directly would bypass Iroh's Discovery
trait integration. We'd lose ConcurrentDiscovery composition and endpoint builder
convenience.

## References

- Iroh Discovery trait: `iroh::discovery::Discovery`
- Pkarr: https://github.com/pubky/pkarr — BEP44 signed DNS records
- iroh-gossip: https://github.com/n0-computer/iroh-gossip — Topic-based pub/sub
- BEP44: http://bittorrent.org/beps/bep_0044.html — Mutable DHT items
- Iroh global node discovery: https://www.iroh.computer/blog/iroh-global-node-discovery
- ADR-0003: Peer Discovery Architecture (Beacon + DHT)
- UserData: `iroh::discovery::UserData` (max 245 bytes, UTF-8)

## Decision Log

- **2026-02-22**: Proposed Iroh/pkarr-based DHT discovery as replacement for
  libp2p-kad approach in ADR-0003 Tier 2
