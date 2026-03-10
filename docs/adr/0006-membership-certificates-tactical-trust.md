# ADR-0006: Membership Certificates and Tactical Trust

> **Provenance**: Transferred from peat repo ADR-048. Renumbered for peat-mesh.

**Status**: Accepted
**Date**: 2025-01-29 (updated 2026-03-09)
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
- [ ] Integration wiring in `peat-mesh-node` binary (register enrollment ALPN + certificate store)
- [ ] End-to-end enrollment flow test (node → authority → cert → CRDT propagation)

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
