# Trust Model & Certificate Validation

This document describes the layered trust model used by peat-mesh for peer
authentication, and how it integrates with peat-registry for OCI sync.

## Layers

### Layer 1: Formation Secret (Transport)

Every mesh formation shares a pre-shared key (`formation_secret`). Nodes derive
deterministic Iroh `EndpointId`s via `HKDF(formation_secret, "iroh:" + hostname)`.
This provides:

- **Peer discovery**: any node can compute any other node's EndpointId from its
  hostname alone.
- **Transport authentication**: HMAC-SHA256 challenge-response during QUIC
  handshake (via `FormationKey`). Nodes that don't possess the secret can't join.

This layer is always active and is the minimum requirement for mesh membership.

### Layer 2: Mesh Certificates (Application)

Optional application-level certificates bind a node's identity to:

| Field                | Purpose                                        |
|---------------------|------------------------------------------------|
| `subject_public_key` | Ed25519 key (from `DeviceKeypair`)             |
| `node_id`            | Hostname, bridges to HKDF-derived EndpointId   |
| `mesh_id`            | Formation identifier                           |
| `tier`               | Enterprise / Regional / Tactical / Edge        |
| `permissions`        | Bitflags: RELAY, EMERGENCY, ENROLL, ADMIN      |
| `issued_at_ms`       | Validity window start                          |
| `expires_at_ms`      | Validity window end (0 = no expiration)        |
| `issuer_public_key`  | Authority that signed this certificate         |
| `signature`          | Ed25519 signature over all fields above        |

Certificates are issued by a trusted authority and stored as wire-encoded files
in the `peer_certificates_dir`. Authority public keys (raw 32 bytes) are stored
in `trusted_authorities_dir`.

### Identity Mapping

The `node_id` field solves a key identity bridging problem:

```
DeviceKeypair  ──►  subject_public_key  (certificate identity)
                        │
               node_id  │  (hostname — bridges the two key spaces)
                        │
HKDF(secret)   ──►  EndpointId          (transport identity)
```

`PeerConnector` discovers peers by hostname and validates them via
`CertificateBundle::validate_node_id(hostname)`. This checks that the hostname
maps to a non-expired, authority-signed certificate.

## Validation Points

### PeerConnector (peat-mesh)

When a `CertificateBundle` is configured:

1. **PeerFound**: validate `node_id` against the bundle before registering.
   If `require_certificates` is true, reject unknown peers. Otherwise, allow
   with a warning.
2. **PeerUpdated**: re-validate. If the certificate has expired or been removed,
   and `require_certificates` is true, remove the peer from Iroh.
3. **PeerLost**: no validation needed (peer is being removed).

### MeshRegistryNode (peat-registry)

`MeshConfig` controls certificate behavior:

```toml
[mesh]
require_peer_certificates = true
trusted_authorities_dir = "/etc/peat/authorities"
peer_certificates_dir = "/etc/peat/certificates"
```

At startup, `MeshRegistryNode` loads authorities and certificates into a
`CertificateBundle`. The bundle is shared via `Arc<RwLock<CertificateBundle>>`
for runtime updates (e.g., adding newly-enrolled peers).

Expired certificates are purged during `connect_peer()` when
`require_peer_certificates` is enabled.

## Enrollment

New nodes obtain certificates through the `EnrollmentService` trait:

```
New Node                          Authority
    │                                  │
    │  EnrollmentRequest               │
    │  (pubkey + node_id + token) ──►  │
    │                                  │  verify signature
    │                                  │  validate token
    │  EnrollmentResponse              │
    │  ◄── (status + certificate)      │
    │                                  │
```

The `StaticEnrollmentService` maps pre-provisioned bootstrap tokens to
(tier, permissions) pairs. On approval, it issues a signed `MeshCertificate`
with the requester's `node_id` embedded.

### Bootstrap Token Distribution

Tokens are opaque secrets distributed out-of-band:

- QR code (printed / ATAK display)
- BLE transfer (peat-btle)
- Pre-provisioned config files
- UDS Registry PAT/OIDC bridge (future)

## Tier Hierarchy

| Tier | Ordinal | Typical Role |
|------|---------|--------------|
| Enterprise | 0 | Data center, source of truth |
| Regional | 1 | Hub, caches from enterprise |
| Tactical | 2 | Field node, intermittent connectivity |
| Edge | 3 | Minimal storage, constrained bandwidth |

Tiers map directly to peat-registry's `RegistryTier` for artifact routing.
Lower ordinal = higher trust and more resources.

## Permission Bits

```
RELAY      0x01  Can relay messages for other nodes
EMERGENCY  0x02  Can trigger emergency alerts
ENROLL     0x04  Can enroll new members (delegation)
ADMIN      0x80  Full administrative privileges

STANDARD   0x03  RELAY | EMERGENCY
AUTHORITY  0x87  All permissions
```

Compatible with peat-protocol's `MemberPermissions` bitflags.

## Wire Formats

### MeshCertificate

```
[subject_pubkey:32][mesh_id_len:1][mesh_id:N][node_id_len:1][node_id:M]
[tier:1][permissions:1][issued_at:8 LE][expires_at:8 LE]
[issuer_pubkey:32][signature:64]
```

Minimum 148 bytes (empty mesh_id and node_id).

### EnrollmentRequest

```
[subject_pubkey:32][mesh_id_len:1][mesh_id:N][node_id_len:1][node_id:P]
[requested_tier:1][token_len:2 LE][token:M][timestamp:8 LE][signature:64]
```

Minimum 109 bytes.
