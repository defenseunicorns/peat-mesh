# ADR-0006: Membership Certificates and Tactical Trust

> **Provenance**: Transferred from peat repo ADR-048. Renumbered for peat-mesh.

**Status**: Accepted
**Date**: 2025-01-29 (updated 2026-03-18)
**Authors**: Codex, Kit Plummer
**Related**: ADR-006 (Security Architecture), ADR-044 (E2E Encryption), ADR-039 (peat-btle Mesh Transport)

## Context

ADR-006 describes a full PKI model with X509 certificates and external CA hierarchies. While appropriate for enterprise deployments, tactical field operations require a simpler, self-contained trust model that:

- Works offline (no external CA connectivity)
- Bootstraps quickly (QR code, NFC tap, bootstrap token)
- Has mesh-local authority (squad leader's device or genesis node)
- Includes callsign binding directly (tactical) or node_id binding (infrastructure)
- Supports time-limited credentials with re-authentication

### Current Problem

Mesh encryption (ChaCha20-Poly1305 via peat-btle) proves a sender has the mesh key, but does NOT authenticate *which* mesh member sent a message. This enables spoofing:

1. Attacker joins mesh (has mesh key)
2. Attacker sends CannedMessage with `sourceNodeId` = victim's nodeId
3. Receiver displays "CHECK IN from DINO" but DINO never sent it

Additionally:
- Callsigns are self-declared (no authority binding)
- No enrollment process for new devices
- No certificate expiration or re-authentication
- No trust hierarchy (all mesh members are equal)
- No path from zero nodes to a functioning mesh (genesis problem)

## Decision

### Tactical Trust Model

Implement a lightweight, mesh-local trust hierarchy with two certificate types spanning the BLE-to-QUIC spectrum:

```
MeshGenesis (root of trust)
    │
    ├── mesh_seed (256-bit, derives encryption/formation secret)
    ├── mesh_id (8 hex chars, derived)
    ├── formation_secret (HKDF-derived, used for Iroh EndpointId)
    ├── auth_interval_hours (e.g., 24)
    ├── grace_period_hours (e.g., 4)
    ├── policy (Open / Controlled / Strict)
    │
    └── authority_keypair (Ed25519)
            │
            ├── signs ──► MembershipCertificate (peat-protocol, BLE/tactical)
            │                 ├── member_public_key
            │                 ├── callsign (authority-assigned)
            │                 ├── mesh_id
            │                 ├── issued_at_ms / expires_at_ms
            │                 ├── permissions (MemberPermissions)
            │                 └── issuer_signature
            │
            └── signs ──► MeshCertificate (peat-mesh, QUIC/infrastructure)
                              ├── subject_public_key
                              ├── mesh_id
                              ├── node_id (hostname, maps to EndpointId)
                              ├── tier (Enterprise/Regional/Tactical/Edge)
                              ├── permissions (byte-compatible)
                              ├── issued_at_ms / expires_at_ms
                              └── issuer_signature
```

### Identity Mapping

A single Ed25519 keypair (`DeviceKeypair`) is the root of a node's identity. All other identifiers are derived:

```
DeviceKeypair (Ed25519)
    │
    ├── public_key_bytes() ──► certificate.subject_public_key
    │
    ├── node_id (hostname) ──► certificate.node_id
    │                          (configured or derived from hostname)
    │
    └── HKDF(formation_secret, "iroh:" + node_id) ──► Iroh EndpointId
                                                       (QUIC transport identity)
```

The `node_id` field bridges certificate identity (Ed25519 public key) to transport identity (HKDF-derived Iroh EndpointId). This allows PeerConnector to validate discovered peers by hostname lookup in the CertificateBundle.

### Permission Bits (byte-compatible across all crates)

```rust
const RELAY:     u8 = 0b0000_0001;  // Can relay messages
const EMERGENCY: u8 = 0b0000_0010;  // Can trigger emergencies
const ENROLL:    u8 = 0b0000_0100;  // Can enroll new members (delegation)
const ADMIN:     u8 = 0b1000_0000;  // Full authority

const STANDARD:  u8 = RELAY | EMERGENCY;
const AUTHORITY:  u8 = RELAY | EMERGENCY | ENROLL | ADMIN;
```

Used identically in peat-protocol (`MemberPermissions`), peat-mesh (`permissions` module), and peat-btle.

### MeshCertificate Structure (peat-mesh)

```rust
pub struct MeshCertificate {
    pub subject_public_key: [u8; 32],
    pub mesh_id: String,
    pub node_id: String,           // hostname → EndpointId bridge
    pub tier: MeshTier,            // Enterprise/Regional/Tactical/Edge
    pub permissions: u8,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,        // 0 = no expiration
    pub issuer_public_key: [u8; 32],
    pub signature: [u8; 64],
}
```

Wire format: `[subject:32][mesh_id_len:1][mesh_id:N][node_id_len:1][node_id:M][tier:1][perms:1][issued:8][expires:8][issuer:32][sig:64]` — minimum 148 bytes.

### MembershipCertificate Structure (peat-protocol)

```rust
pub struct MembershipCertificate {
    pub member_public_key: [u8; 32],
    pub callsign: String,          // authority-assigned, max 16 UTF-8 bytes
    pub mesh_id: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub permissions: MemberPermissions,
    pub issuer_public_key: [u8; 32],
    pub issuer_signature: [u8; 64],
}
```

Wire format: ~145 bytes (32 + 1 + callsign + 8 + 8 + 8 + 1 + 32 + 64).

---

## Zero-to-Mesh Lifecycle

### Phase 0: Genesis

The genesis event creates all cryptographic material needed to bootstrap a mesh from nothing.

```
Operator runs `peat genesis create --name "ALPHA-TEAM" --policy Controlled`

    ┌─────────────────────────────────────────┐
    │            MeshGenesis                   │
    │                                          │
    │  mesh_name: "ALPHA-TEAM"                 │
    │  mesh_seed: [32 bytes, CSPRNG]           │
    │  policy: Controlled                      │
    │  created_at_ms: <now>                    │
    │                                          │
    │  Derived:                                │
    │    mesh_id = BLAKE3_keyed(seed, name)    │
    │    formation_secret = BLAKE3_kdf(seed)   │
    │    encryption_secret = BLAKE3_kdf(seed)  │
    │    beacon_key_base = BLAKE3_kdf(seed)    │
    │                                          │
    │  authority_keypair: Ed25519 (genesis)     │
    │  root_cert: self-signed MeshCertificate  │
    │             (tier=Enterprise, perms=ALL)  │
    └─────────────────────────────────────────┘
```

The genesis artifact is stored locally by the creator. `MeshCredentials` (no private key) can be shared with nodes joining the mesh.

**Existing implementation**: `peat-btle/src/security/genesis.rs` — `MeshGenesis::create()`, `MeshCredentials::from_genesis()`.

### Phase 1: Static Provisioning (Enterprise / K8s)

For infrastructure deployments where all nodes are known ahead of time:

```
                     ┌──────────────────┐
                     │  Offline Staging  │
                     │  (operator CLI)   │
                     └────────┬─────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
  ┌──────────┐        ┌──────────┐          ┌──────────┐
  │ Node A   │        │ Node B   │          │ Node C   │
  │          │        │          │          │          │
  │ cert_a   │        │ cert_b   │          │ cert_c   │
  │ auth_key │        │ auth_key │          │ auth_key │
  │ form_sec │        │ form_sec │          │ form_sec │
  └──────────┘        └──────────┘          └──────────┘
```

1. Operator generates genesis + per-node keypairs offline
2. Pre-signs a `MeshCertificate` for each node
3. Deploys via Helm values, Kubernetes secrets, or config management:
   - Authority public key → `trusted_authorities_dir/`
   - Node certificate → `peer_certificates_dir/`
   - Formation secret → env var or secret mount
4. All nodes start with full trust — immediate mesh formation
5. `CertificateBundle::load_authorities_from_dir()` + `load_certificates_from_dir()` at startup

**Use cases**: Data center registries, regional hubs, CI/CD pipelines, Zarf packages.

### Phase 2: Dynamic Enrollment (Two-Phase QUIC)

For tactical and edge deployments where nodes appear dynamically:

#### The Enrollment Deadlock Problem

A new node needs a certificate to participate in the mesh, but needs to connect to the mesh to get a certificate. This is solved by a two-phase connection model using QUIC ALPN protocol negotiation:

```
New Node (B)                              Authority Node (A)
    │                                              │
    │ ── Layer 0: QUIC connect ──────────────────► │
    │    (formation_secret → EndpointId)           │
    │    (transport-level trust only)              │
    │                                              │
    │ ── Layer 1: ALPN "peat-enroll/1" ─────────► │
    │    EnrollmentRequest {                       │
    │      subject_public_key,                     │
    │      mesh_id, node_id,                       │
    │      bootstrap_token,                        │
    │      signature                               │
    │    }                                         │
    │                                              │
    │ ◄── EnrollmentResponse ──────────────────── │
    │    {                                         │
    │      status: Approved,                       │
    │      certificate: MeshCertificate,           │
    │      formation_secret: Some(...)             │
    │    }                                         │
    │                                              │
    │ ── Layer 2: ALPN "peat-sync/1" ───────────► │
    │    (Automerge CRDT sync)                     │
    │    (certificate REQUIRED)                    │
    │                                              │
```

**Layer 0 — Transport**: QUIC connection via Iroh. Formation secret provides HKDF-derived EndpointId. This proves the node knows the formation secret (was given it out-of-band), but does NOT authenticate which node it is.

**Layer 1 — Enrollment ALPN**: A dedicated QUIC application protocol (`peat-enroll/1`) that handles `EnrollmentRequest`/`EnrollmentResponse` exchange. No certificate required. Bootstrap token (from QR code, pre-shared config, or out-of-band) validates the request.

**Layer 2 — Automerge ALPN**: The data sync protocol (`peat-sync/1`). Certificate REQUIRED. Only enrolled nodes can participate in CRDT document sync, which carries application data, topology state, and certificate gossip.

#### Bootstrap Token Distribution

| Method | Environment | Flow |
|--------|-------------|------|
| QR code | Tactical (ATAK) | Authority displays QR → device scans |
| NFC tap | Tactical (NPE) | Authority tap → device receives token |
| USB/serial | Staging | Pre-load during hardware prep |
| Config file | Enterprise | Helm values, K8s secrets |
| BLE exchange | Wearable (WearTAK) | BLE pairing → token transfer |

### Phase 3: CRDT Certificate Distribution and Quorum

After enrollment, certificates must propagate to all mesh members so they can validate each other:

```
Timeline:

T0: Genesis node (A) starts with root cert
    Mesh: {A}

T1: Node B connects, enrolls via ALPN
    A signs cert_B, stores in Automerge doc
    Mesh: {A, B}  — B has cert_A (root), A has cert_B

T2: Node C connects to A, enrolls
    A signs cert_C, writes to CRDT doc
    CRDT sync propagates cert_C to B
    Mesh: {A, B, C}  — all nodes have all certs

T3: A grants ENROLL permission to B
    B can now enroll new nodes (delegation)
    A can go offline — mesh is self-sustaining

T4: Node D connects to B (not A)
    B enrolls D (has ENROLL permission)
    B writes cert_D to CRDT doc
    CRDT gossip propagates cert_D to A, C
    Mesh: {A, B, C, D}  — quorum maintained without A
```

#### CRDT Certificate Document

Certificates are stored in a well-known Automerge document:

```
automerge_doc["certificates"] = {
    "<subject_pubkey_hex>": <wire-encoded MeshCertificate>,
    ...
}
automerge_doc["revocations"] = {
    "<subject_pubkey_hex>": { reason: String, timestamp_ms: u64 },
    ...
}
```

Nodes watch for CRDT changes and hot-reload their local `CertificateBundle`. On merge:
1. Decode new certificate entries
2. Verify signature against trusted authorities
3. Add to local `CertificateBundle`
4. Check revocations list, remove revoked certs

#### Authority Bridge Rule

The authority node has a special rule: it allows Layer 2 (Automerge) connections from nodes it has *just enrolled*, even before other nodes know about them. This bootstraps the CRDT sync that distributes the new certificate to the rest of the mesh.

#### Delegation Enforcement

When a non-root node signs a certificate (delegated enrollment):
1. Verify the signing node has `ENROLL` permission in its own certificate
2. Verify the signing node's certificate is valid and not expired
3. The delegated cert's issuer_public_key is the delegating node (not root)
4. Any node can verify the chain: root → delegator → new node

### Mesh-Level Auth Policy

MeshGenesis includes authentication policy:

```rust
pub struct MeshGenesis {
    // ... existing fields ...
    pub auth_interval_hours: u16,  // Re-authentication interval (0 = no expiry)
    pub grace_period_hours: u16,   // Grace period after expiration
}
```

**Recommended defaults**: 24-hour auth interval, 4-hour grace period.

### Graceful Degradation

**During grace period** (cert expired but within grace):
- Device shows prominent "AUTH EXPIRED" warning
- Device continues to operate (send/receive)
- Other devices see this device flagged as "stale cert"
- Authority can still re-issue certificate

**After grace period**:
- Hard cutoff - device cannot participate in mesh
- Must re-enroll from scratch (or authority manually extends)

### Callsign Assignment (Tactical)

Authority has two options:
1. **Manual entry** - Type callsign directly
2. **Randomize** - Generate NATO phonetic + 2-digit (e.g., ALPHA-01, ZULU-47)

NATO + 2-digit provides 2,600 unique callsigns with familiar, radio-clear format.

Callsigns are carried in `MembershipCertificate` (peat-protocol) for BLE/tactical use. `MeshCertificate` (peat-mesh) uses `node_id` instead for infrastructure identity.

### Message Authentication

All messages include Ed25519 signature from sender:

```
[marker:1][payload:N][signature:64]
```

Receive flow:
1. Decrypt with mesh key (proves mesh membership)
2. Lookup `source_node_id` in certificate cache
3. If unknown → reject message
4. Verify signature using cached public key
5. Display with verified callsign from certificate

### Re-Authentication Flow

```
Timeline:
├── T-0: Certificate issued
├── T-23h: Warning "Re-auth in 1 hour"
├── T-24h: Expiration (grace period starts)
├── T-28h: Hard cutoff (if grace = 4h)
└── Re-auth succeeds: New cert issued, timer resets

Protocol:
1. Device connects to authority (or any node with ENROLL)
2. Sends current certificate + fresh attestation
3. Authority verifies device is still authorized
4. Authority issues new certificate (same callsign/node_id, new expiration)
5. New cert propagates via CRDT gossip
```

### Certificate Exchange During Discovery

**BLE (peat-btle):**
1. Exchange IdentityAttestations (108 bytes each) - existing protocol
2. Exchange MembershipCertificates (~145 bytes each)
3. Each node verifies other's certificate against `genesis.creator_public_key`
4. Store in peer registry: `node_id → (public_key, callsign, certificate)`

**QUIC (peat-mesh):**
1. QUIC connection established via formation_secret (Layer 0)
2. Enrollment ALPN if no certificate (Layer 1)
3. Certificate validated by PeerConnector against CertificateBundle
4. Automerge sync enabled only for validated peers (Layer 2)

Unknown or invalid certificates → reject peer, do not sync.

## Operational Scenarios

The following scenarios arise in real-world tactical and infrastructure deployments. Each must be addressed by the trust model — either by existing mechanisms, defined procedures, or acknowledged future work.

### Mesh Merge / Federation

Two independently-created meshes (e.g., two squads converging on an objective) need to interoperate without re-enrolling every device.

**Problem**: Each mesh has its own `MeshGenesis`, authority keypair, formation secret, and certificate chain. There is no shared root of trust.

**Approach**: Temporary federation via cross-signed bridge certificates.

1. Authorities (or ENROLL delegates) from each mesh establish a direct link (BLE, QUIC, or physical QR exchange)
2. Each authority issues a **federation certificate** for the other's authority public key, with:
   - Limited permissions (e.g., RELAY only — no ENROLL, no ADMIN)
   - Short TTL (hours, not days)
   - A `federation_mesh_id` field distinguishing foreign certs from native ones
3. Bridge certificates propagate via CRDT gossip within each mesh
4. Nodes accept messages from federated peers at reduced trust (display "FEDERATED" indicator, no access to local mesh administrative functions)
5. Federation dissolves on certificate expiry — no persistent coupling

**Open questions**: Message routing between federated meshes (relay policy). Whether federated nodes can participate in CRDT document sync or only relay opaque messages. Priority relative to native mesh traffic.

### Split-Brain / Partition Recovery

A mesh partitions (e.g., terrain blocks radio, network outage). Both halves continue operating. ENROLL delegates in each partition may issue new certificates independently.

**Problem**: When partitions rejoin, CRDT merge handles data convergence, but both halves may have enrolled new nodes the other half has never seen. In adversarial scenarios, a captured partition could have issued certs to hostile nodes.

**Approach**: Convergence via CRDT merge with authority reconciliation.

1. **Certificate merge is automatic**: CRDT sync propagates all certificates from both partitions. Each node validates incoming certs against the chain of trust (root → delegator → new node). Valid chains are accepted regardless of which partition issued them.
2. **Conflicting revocations**: If partition A revoked a node that partition B kept active, the revocation wins (revocations are tombstones in the CRDT — once written, they persist). CRDT merge semantics: `revocations` map entries are never deleted, only added.
3. **Suspicious enrollment detection**: After rejoin, nodes with ADMIN permission should review newly-merged certificates for anomalies (unexpected delegator, unusual permissions, certs issued during known compromise window). This is an operational procedure, not an automated mechanism.
4. **Epoch counter**: Each enrollment increments a mesh-wide epoch counter in the CRDT. After partition rejoin, a gap or fork in the epoch sequence signals that independent enrollment occurred — operators can audit.

**Not addressed**: Automated conflict resolution for duplicate `node_id` assignments across partitions (unlikely with UUID-based IDs, possible with hostname-based IDs).

### Authority Loss Without Delegation

The authority device is destroyed, lost, or captured before delegating ENROLL permission to any other node.

**Problem**: The mesh is frozen for enrollment. Existing members continue operating (their certs are still valid), but no new devices can join and expired certs cannot be renewed.

**Approach**: Genesis backup and M-of-N recovery.

1. **Genesis export**: At genesis time, the operator exports an encrypted backup of `MeshGenesis` (including `authority_keypair` private key). Storage options:
   - Split into M-of-N Shamir shares distributed to trusted personnel
   - Encrypted to a hardware security key (YubiKey, CAC) held by a commander
   - Stored in a secure offline vault (USB, air-gapped system)
2. **Recovery procedure**: Any device can import the genesis backup, reconstruct the authority keypair, and resume enrollment. The recovered authority self-signs a new root cert with the same public key — existing certificate chains remain valid.
3. **Operational discipline**: Standing guidance should be to delegate ENROLL to at least two nodes immediately after genesis. Genesis backup is a last resort, not a primary mechanism.

**Not addressed**: Automated authority election or promotion. Intentionally omitted — authority promotion without out-of-band verification is an attack vector. Recovery must involve human decision-making.

### Compromised Formation Secret

The pre-shared `formation_secret` leaks to an adversary (captured device, exfiltrated config, insider threat).

**Problem**: The adversary can establish Layer 0 (QUIC transport) connections to any mesh node. They cannot access Layer 2 data without a valid certificate, but they can:
- Enumerate mesh members by connecting and observing enrollment rejections
- Attempt resource exhaustion via connection floods
- Perform traffic analysis (who connects to whom, when)

**Approach**: Formation secret rotation.

1. **Detection**: Anomalous Layer 0 connections from unknown EndpointIds that fail Layer 1/2 validation. Nodes should log and rate-limit unauthenticated connection attempts.
2. **Rotation trigger**: Authority (or ADMIN node) writes a `formation_secret_rotation` entry to the CRDT certificate document:
   ```
   { new_formation_secret_encrypted: <encrypted to each member's public key>,
     rotation_epoch: <monotonic counter>,
     effective_at_ms: <future timestamp, e.g., now + 5 minutes> }
   ```
3. **Rollover**: Each node decrypts its copy of the new formation secret, schedules a switchover at `effective_at_ms`. During a brief transition window, nodes accept connections on both old and new secrets. After the window closes, old secret is discarded.
4. **Stragglers**: Nodes that were offline during rotation will fail to connect on the old secret. They must re-enroll via out-of-band bootstrap (QR, NFC, pre-shared config with the new secret).

**Not addressed**: Rotation of derived keys (encryption_secret, beacon_key_base) — these should rotate in lockstep with formation_secret. Exact CRDT schema for the rotation document.

### Device Transfer / Reprovisioning

A device changes roles (relay → user node), is reassigned to a different operator, or needs permission changes without full re-enrollment.

**Problem**: The current model requires re-enrollment to change permissions or callsign. Re-enrollment means a new certificate, but the device's keypair and mesh membership are unchanged.

**Approach**: Certificate reissuance by authority.

1. Authority (or ENROLL delegate) issues a **replacement certificate** for the same `subject_public_key` with updated fields (permissions, callsign, tier, expiration).
2. The new certificate is written to the CRDT certificate document, overwriting the previous entry (keyed by `subject_pubkey_hex`).
3. The old certificate is implicitly superseded — nodes use the highest-epoch cert for a given public key.
4. **No re-enrollment needed**: The device already has a valid keypair and formation secret. The authority simply signs a new cert and publishes it via CRDT.

**Constraint**: Only the original issuer (or a higher-tier authority) can reissue. A delegated ENROLL node cannot reissue certificates originally signed by root — this prevents privilege escalation.

### Contested / Denied Environments

An adversary captures a device with valid credentials (formation_secret + unexpired certificate). The adversary may also be jamming communications, preventing revocation propagation.

**Problem**: Revocation requires CRDT gossip to reach all nodes. If the adversary is jamming, legitimate nodes cannot receive the revocation, and the compromised device continues to be trusted.

**Approach**: Defense in depth — no single mechanism is sufficient.

1. **Short certificate TTL**: In high-threat environments, reduce `auth_interval_hours` to 4-8 hours. Captured certs expire quickly even without revocation. Grace period should be minimal (1 hour or less).
2. **Local revocation lists**: Nodes that directly observe suspicious behavior (e.g., duplicate source_node_id from different EndpointIds, messages failing signature verification, unexpected geographic location via beacon) can locally blacklist a peer without waiting for CRDT propagation.
3. **Revocation propagation on reconnect**: When jamming subsides and nodes reconnect, CRDT merge immediately propagates any revocations written during the blackout. Revocations are tombstones — they always win on merge.
4. **Formation secret rotation**: After the compromised device is identified, rotate the formation secret (see above). Even if the adversary has the old cert, they lose Layer 0 transport access.
5. **Operational procedure**: Report compromise over voice/out-of-band. All nodes manually blacklist the compromised node_id until CRDT propagation confirms revocation.

**Accepted risk**: During the window between compromise and revocation propagation (or cert expiry), the adversary can impersonate the captured device. Short TTLs minimize this window. Message replay is not addressed here (see ADR-044 for sequence numbers and anti-replay).

### Multi-Mesh Devices

A gateway node participates in two or more meshes simultaneously — e.g., bridging a tactical BLE mesh to a regional QUIC hub, or a relay node spanning two squad meshes.

**Problem**: The device holds credentials for multiple genesis roots. Identity, certificate validation, and message routing must be isolated per mesh to prevent cross-contamination.

**Approach**: Per-mesh credential isolation.

1. Each mesh context is a separate `MeshCredentials` instance with its own formation secret, authority chain, and certificate bundle. No shared state between mesh contexts.
2. The device holds a separate `DeviceKeypair` per mesh (or derives mesh-specific keypairs from a root key via HKDF with mesh_id as context). This ensures EndpointIds are unique per mesh.
3. **Transport isolation**: Separate QUIC endpoints (different ports) per mesh. No cross-mesh connection reuse.
4. **Message relay policy**: Messages are never automatically forwarded between meshes. A gateway application explicitly decides what to bridge, applying policy (classification, need-to-know, redaction) at the application layer.
5. **Certificate isolation**: A certificate valid in mesh A has no meaning in mesh B. The gateway authenticates independently in each mesh.

**Open questions**: Unified device management (single UI showing both meshes). Resource contention (CPU, radio time) between mesh contexts. Whether federation certificates (see Mesh Merge above) could replace dedicated gateway nodes for some use cases.

### Clock Skew in GPS-Denied Environments

Certificate expiration relies on `issued_at_ms` and `expires_at_ms`. Nodes in GPS-denied or underground environments may have significant clock drift.

**Problem**: A node with a slow clock may accept expired certificates. A node with a fast clock may reject valid certificates prematurely. There is no trusted time source.

**Approach**: Bounded tolerance with mesh-relative time.

1. **Skew tolerance**: Certificate validation applies a configurable tolerance window (default: 30 minutes). A certificate is considered valid if `now` is within `[issued_at_ms - tolerance, expires_at_ms + tolerance]`.
2. **Mesh time gossip**: Nodes include their local timestamp in heartbeat messages (graph cache entries already carry `heartbeat_ms`). Each node computes the median timestamp across recent heartbeats from peers and uses it as a mesh-relative reference. Outliers (>1 hour from median) are flagged.
3. **No hard enforcement for short skew**: If a node's clock is within tolerance, it operates normally. If outside tolerance, it logs a warning and continues with degraded trust (peers see "CLOCK SKEW" flag alongside the node's graph entry).
4. **Grace period extension**: The existing grace period mechanism (4 hours default) already absorbs moderate clock drift. In GPS-denied environments, operators should increase `grace_period_hours` proportionally.

**Not addressed**: Byzantine clock attacks (a malicious node deliberately advertising false timestamps to manipulate mesh-relative time). Mitigated by using median rather than mean, and by weighting timestamps from higher-tier nodes.

## Consequences

### Positive

- **Offline operation**: No external CA needed, mesh-local authority
- **Fast enrollment**: QR scan + BLE exchange (<30s) or bootstrap token over QUIC
- **Spoofing prevention**: Messages cryptographically bound to sender identity
- **Callsign integrity**: Authority controls who uses which callsign
- **Automatic cleanup**: Expired certs drop compromised/lost devices
- **Simple key management**: Ed25519 only, no X509 complexity
- **Self-sustaining**: ENROLL delegation allows authority to go offline
- **Certificate distribution**: CRDT gossip ensures all nodes converge on the same trust state
- **Two deployment modes**: Static provisioning (enterprise) and dynamic enrollment (tactical) using the same certificate types

### Negative

- **Single point of authority**: If authority device is lost before delegation, no new enrollments
  - Mitigation: Delegation via ENROLL permission flag; multiple ENROLL delegates
- **Certificate size**: 145-148 bytes per peer, adds to discovery overhead
  - Acceptable for BLE mesh (<10 peers typical) and negligible for QUIC
- **Breaking change**: New wire formats not backward compatible
  - Mitigation: Version negotiation, legacy mode for testing
- **CRDT document size**: Grows with number of certificates (mitigated by expiration/cleanup)

### Neutral

- Coexists with ADR-006 PKI model for enterprise deployments
- peat-btle provides crypto primitives, peat-mesh provides QUIC-layer certs, peat-protocol bridges both
- Permission bits are byte-compatible across all crates

## Implementation

### Layer Responsibilities

| Layer | Responsibility |
|-------|----------------|
| **peat-btle** | Ed25519 primitives, MeshGenesis, DeviceIdentity, IdentityAttestation, MembershipPolicy |
| **peat-lite** | Signed wire formats (86-byte CannedMessage) |
| **peat-mesh** | MeshCertificate, CertificateBundle, EnrollmentRequest/Response, enrollment ALPN handler, CRDT cert document, PeerConnector validation |
| **peat-protocol** | MembershipCertificate, CertificateRegistry, callsign binding, BLE token conversion |
| **peat-registry** | MeshConfig with certificate paths, CertificateBundle loading at startup, connect_peer validation |
| **Apps** | Enrollment UI, QR display/scan, callsign assignment |

### Existing Implementations

| Crate | Module | Status |
|-------|--------|--------|
| peat-btle | `src/security/genesis.rs` | MeshGenesis, MeshCredentials, BLAKE3 key derivation |
| peat-btle | `src/security/identity.rs` | DeviceIdentity, IdentityAttestation |
| peat-mesh | `src/security/certificate.rs` | MeshCertificate, CertificateBundle, delegation chain verification |
| peat-mesh | `src/security/enrollment.rs` | EnrollmentRequest/Response, StaticEnrollmentService |
| peat-mesh | `src/security/genesis.rs` | MeshGenesis, MeshCredentials, MembershipPolicy, HKDF key derivation |
| peat-mesh | `src/storage/enrollment_transport.rs` | EnrollmentProtocolHandler (`peat/enroll/1` ALPN), request_enrollment() |
| peat-mesh | `src/storage/certificate_store.rs` | CertificateStore (CRDT-backed), hot-reload, revocation propagation |
| peat-mesh | `src/storage/mesh_sync_transport.rs` | SyncProtocolHandler with Layer 2 certificate gating |
| peat-mesh | `src/peer_connector.rs` | Certificate validation on PeerFound/PeerUpdated |
| peat-mesh | `docs/trust-model.md` | Trust model documentation |
| peat-protocol | `src/security/membership.rs` | MembershipCertificate, CertificateRegistry |
| peat-registry | `src/config.rs` | MeshConfig with cert paths, EnrollmentConfig |
| peat-registry | `src/mesh/node.rs` | CertificateBundle loading, connect_peer validation |

### Remaining Work

- [x] MeshGenesis in peat-mesh (`src/security/genesis.rs`)
- [x] Enrollment ALPN protocol handler (`src/storage/enrollment_transport.rs`, `peat/enroll/1`)
- [x] CRDT certificate document (`src/storage/certificate_store.rs`)
- [x] CertificateBundle hot-reload from CRDT changes (`CertificateStore::watch_and_reload()`)
- [x] Two-phase connection gating (Layer 2 cert validation in `SyncProtocolHandler`)
- [x] Delegation chain verification (ENROLL permission check in `CertificateBundle::is_trusted_issuer()`)
- [x] Certificate revocation propagation via CRDT (`CertificateStore::publish_revocation()`)
- [x] Integration wiring in `peat-mesh-node` binary (register enrollment ALPN + certificate store)
- [x] End-to-end enrollment flow test (node → authority → cert → CRDT propagation)

### Related Issues

- **peat-btle** `cac154cc`: Crypto primitives (sign/verify utilities)
- **peat-lite** `2140b82a`: Signed CannedMessage wire format
- **Peat** `23505348`: Membership certificates, enrollment, trust hierarchy
- **peat-mesh/peat-registry** `#592`: Certificate validation, enrollment, trust hierarchy

## References

- peat-btle `src/security/identity.rs`: DeviceIdentity, IdentityAttestation
- peat-btle `src/security/genesis.rs`: MeshGenesis, MembershipPolicy
- peat-mesh `src/security/certificate.rs`: MeshCertificate, CertificateBundle
- peat-mesh `src/security/enrollment.rs`: EnrollmentRequest/Response, StaticEnrollmentService
- peat-mesh `docs/trust-model.md`: Trust model and validation documentation
- ADR-006: Security, Authentication, Authorization (PKI model)
- ADR-044: E2E Encryption and Key Management (MLS, group keys)
