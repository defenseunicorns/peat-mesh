//! Mesh-layer peer certificates and trust validation.
//!
//! Provides lightweight certificate types for peer authentication at the mesh
//! transport layer. Higher-level certificate types (e.g., `MembershipCertificate`
//! in peat-protocol) can convert to/from these types.
//!
//! # Trust Model
//!
//! ```text
//! Authority (root)
//!     │
//!     └── signs ──► MeshCertificate
//!                       ├── subject_public_key ──► derives EndpointId
//!                       ├── mesh_id (formation identifier)
//!                       ├── tier (Enterprise/Regional/Tactical/Edge)
//!                       ├── permissions (bitflags)
//!                       └── validity window (issued_at..expires_at)
//! ```

use std::collections::HashMap;
use std::path::Path;

use ed25519_dalek::{Verifier, VerifyingKey};
use tracing::{debug, warn};

use super::error::SecurityError;
use super::keypair::DeviceKeypair;

/// Tier classification for mesh nodes.
///
/// Mirrors the distribution hierarchy used by peat-registry for
/// artifact routing. Lower ordinal = higher trust / more resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum MeshTier {
    /// Tier 0 — enterprise data center, source of truth.
    Enterprise = 0,
    /// Tier 1 — regional hub, caches from enterprise.
    Regional = 1,
    /// Tier 2 — tactical node, intermittent connectivity.
    Tactical = 2,
    /// Tier 3 — edge device, minimal storage.
    Edge = 3,
}

impl MeshTier {
    /// Parse from a string (case-insensitive).
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "enterprise" => Some(Self::Enterprise),
            "regional" => Some(Self::Regional),
            "tactical" => Some(Self::Tactical),
            "edge" => Some(Self::Edge),
            _ => None,
        }
    }

    /// Get the tier name as a string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Enterprise => "Enterprise",
            Self::Regional => "Regional",
            Self::Tactical => "Tactical",
            Self::Edge => "Edge",
        }
    }

    /// Encode as a single byte.
    pub fn to_byte(self) -> u8 {
        self as u8
    }

    /// Decode from a single byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Enterprise),
            1 => Some(Self::Regional),
            2 => Some(Self::Tactical),
            3 => Some(Self::Edge),
            _ => None,
        }
    }
}

impl std::fmt::Display for MeshTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Permission flags for mesh certificates.
///
/// Compatible with peat-protocol's `MemberPermissions` bitflags.
pub mod permissions {
    /// Can relay messages for other nodes.
    pub const RELAY: u8 = 0b0000_0001;
    /// Can trigger emergency alerts.
    pub const EMERGENCY: u8 = 0b0000_0010;
    /// Can enroll new members (delegation).
    pub const ENROLL: u8 = 0b0000_0100;
    /// Full administrative privileges.
    pub const ADMIN: u8 = 0b1000_0000;
    /// Standard member: RELAY + EMERGENCY.
    pub const STANDARD: u8 = RELAY | EMERGENCY;
    /// Authority: all permissions.
    pub const AUTHORITY: u8 = RELAY | EMERGENCY | ENROLL | ADMIN;
}

/// A signed certificate binding a public key to mesh membership.
///
/// Wire format (variable length):
/// ```text
/// [subject_pubkey:32][mesh_id_len:1][mesh_id:N][node_id_len:1][node_id:M]
/// [tier:1][permissions:1][issued_at:8][expires_at:8][issuer_pubkey:32][signature:64]
/// ```
/// Minimum size: 148 bytes (with empty mesh_id and node_id).
#[derive(Clone, Debug)]
pub struct MeshCertificate {
    /// Subject's Ed25519 public key (the node this cert is issued to).
    pub subject_public_key: [u8; 32],
    /// Mesh/formation identifier.
    pub mesh_id: String,
    /// Node identifier within the mesh (maps to discovery hostname).
    /// Used to bridge between certificate identity and HKDF-derived Iroh EndpointId.
    pub node_id: String,
    /// Node's tier in the distribution hierarchy.
    pub tier: MeshTier,
    /// Permission bitflags (see [`permissions`] module).
    pub permissions: u8,
    /// Issuance time (Unix epoch milliseconds).
    pub issued_at_ms: u64,
    /// Expiration time (Unix epoch milliseconds). 0 = no expiration.
    pub expires_at_ms: u64,
    /// Issuer's (authority's) Ed25519 public key.
    pub issuer_public_key: [u8; 32],
    /// Ed25519 signature over the signable portion of the certificate.
    pub signature: [u8; 64],
}

impl MeshCertificate {
    /// Create a new unsigned certificate.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        subject_public_key: [u8; 32],
        mesh_id: String,
        node_id: String,
        tier: MeshTier,
        permissions: u8,
        issued_at_ms: u64,
        expires_at_ms: u64,
        issuer_public_key: [u8; 32],
    ) -> Self {
        Self {
            subject_public_key,
            mesh_id,
            node_id,
            tier,
            permissions,
            issued_at_ms,
            expires_at_ms,
            issuer_public_key,
            signature: [0u8; 64],
        }
    }

    /// Create a self-signed root (authority) certificate.
    pub fn new_root(
        authority: &DeviceKeypair,
        mesh_id: String,
        node_id: String,
        tier: MeshTier,
        issued_at_ms: u64,
        expires_at_ms: u64,
    ) -> Self {
        let pubkey = authority.public_key_bytes();
        let mut cert = Self::new(
            pubkey,
            mesh_id,
            node_id,
            tier,
            permissions::AUTHORITY,
            issued_at_ms,
            expires_at_ms,
            pubkey,
        );
        cert.sign_with(authority);
        cert
    }

    /// Sign the certificate in-place with the given keypair.
    pub fn sign_with(&mut self, issuer: &DeviceKeypair) {
        let signable = self.signable_bytes();
        let sig = issuer.sign(&signable);
        self.signature = sig.to_bytes();
    }

    /// Return a signed copy (builder pattern).
    pub fn signed(mut self, issuer: &DeviceKeypair) -> Self {
        self.sign_with(issuer);
        self
    }

    /// Verify the certificate's signature against the issuer public key.
    pub fn verify(&self) -> Result<(), SecurityError> {
        let vk = VerifyingKey::from_bytes(&self.issuer_public_key)
            .map_err(|e| SecurityError::InvalidPublicKey(e.to_string()))?;
        let sig = ed25519_dalek::Signature::from_bytes(&self.signature);
        let signable = self.signable_bytes();
        vk.verify(&signable, &sig)
            .map_err(|e| SecurityError::InvalidSignature(e.to_string()))
    }

    /// Check if this is a self-signed root certificate.
    pub fn is_root(&self) -> bool {
        self.subject_public_key == self.issuer_public_key
    }

    /// Check if the certificate is valid at the given time.
    pub fn is_valid(&self, now_ms: u64) -> bool {
        now_ms >= self.issued_at_ms && (self.expires_at_ms == 0 || now_ms < self.expires_at_ms)
    }

    /// Check if a specific permission is set.
    pub fn has_permission(&self, perm: u8) -> bool {
        self.permissions & perm == perm
    }

    /// Milliseconds remaining until expiration. 0 if expired or no expiration.
    pub fn time_remaining_ms(&self, now_ms: u64) -> u64 {
        if self.expires_at_ms == 0 {
            return u64::MAX;
        }
        self.expires_at_ms.saturating_sub(now_ms)
    }

    /// The bytes that are signed (everything except the signature itself).
    fn signable_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(83 + self.mesh_id.len() + self.node_id.len());
        buf.extend_from_slice(&self.subject_public_key);
        buf.push(self.mesh_id.len() as u8);
        buf.extend_from_slice(self.mesh_id.as_bytes());
        buf.push(self.node_id.len() as u8);
        buf.extend_from_slice(self.node_id.as_bytes());
        buf.push(self.tier.to_byte());
        buf.push(self.permissions);
        buf.extend_from_slice(&self.issued_at_ms.to_le_bytes());
        buf.extend_from_slice(&self.expires_at_ms.to_le_bytes());
        buf.extend_from_slice(&self.issuer_public_key);
        buf
    }

    /// Encode to wire format.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = self.signable_bytes();
        buf.extend_from_slice(&self.signature);
        buf
    }

    /// Decode from wire format.
    pub fn decode(data: &[u8]) -> Result<Self, SecurityError> {
        // Minimum: 32 + 1 + 0 + 1 + 0 + 1 + 1 + 8 + 8 + 32 + 64 = 148
        if data.len() < 148 {
            return Err(SecurityError::SerializationError(format!(
                "certificate too short: {} bytes (min 148)",
                data.len()
            )));
        }

        let mut pos = 0;

        let mut subject_public_key = [0u8; 32];
        subject_public_key.copy_from_slice(&data[pos..pos + 32]);
        pos += 32;

        let mesh_id_len = data[pos] as usize;
        pos += 1;

        if pos + mesh_id_len >= data.len() {
            return Err(SecurityError::SerializationError(
                "certificate truncated at mesh_id".to_string(),
            ));
        }

        let mesh_id = String::from_utf8(data[pos..pos + mesh_id_len].to_vec())
            .map_err(|e| SecurityError::SerializationError(format!("invalid mesh_id: {e}")))?;
        pos += mesh_id_len;

        let node_id_len = data[pos] as usize;
        pos += 1;

        if pos + node_id_len + 1 + 1 + 8 + 8 + 32 + 64 > data.len() {
            return Err(SecurityError::SerializationError(
                "certificate truncated at node_id".to_string(),
            ));
        }

        let node_id = String::from_utf8(data[pos..pos + node_id_len].to_vec())
            .map_err(|e| SecurityError::SerializationError(format!("invalid node_id: {e}")))?;
        pos += node_id_len;

        let tier = MeshTier::from_byte(data[pos])
            .ok_or_else(|| SecurityError::SerializationError("invalid tier byte".to_string()))?;
        pos += 1;

        let permissions = data[pos];
        pos += 1;

        let issued_at_ms = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let expires_at_ms = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let mut issuer_public_key = [0u8; 32];
        issuer_public_key.copy_from_slice(&data[pos..pos + 32]);
        pos += 32;

        let mut signature = [0u8; 64];
        signature.copy_from_slice(&data[pos..pos + 64]);

        Ok(Self {
            subject_public_key,
            mesh_id,
            node_id,
            tier,
            permissions,
            issued_at_ms,
            expires_at_ms,
            issuer_public_key,
            signature,
        })
    }
}

/// A bundle of trusted authority keys and peer certificates.
///
/// Used to validate peer connections and determine peer tier/permissions.
/// Maintains a dual index: by subject public key and by node_id (hostname).
#[derive(Debug, Default)]
pub struct CertificateBundle {
    /// Trusted authority public keys.
    authorities: Vec<[u8; 32]>,
    /// Certificates indexed by subject public key.
    certificates: HashMap<[u8; 32], MeshCertificate>,
    /// Reverse index: node_id → subject public key.
    node_id_index: HashMap<String, [u8; 32]>,
}

impl CertificateBundle {
    /// Create an empty bundle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a trusted authority public key.
    pub fn add_authority(&mut self, public_key: [u8; 32]) {
        if !self.authorities.contains(&public_key) {
            self.authorities.push(public_key);
        }
    }

    /// Add a certificate to the bundle.
    ///
    /// The certificate's signature is verified against the trusted authorities.
    /// Returns an error if the issuer is not trusted or the signature is invalid.
    pub fn add_certificate(&mut self, cert: MeshCertificate) -> Result<(), SecurityError> {
        // Verify signature
        cert.verify()?;

        // Check issuer is trusted (or self-signed root)
        if !cert.is_root() && !self.authorities.contains(&cert.issuer_public_key) {
            return Err(SecurityError::CertificateError(
                "issuer not in trusted authorities".to_string(),
            ));
        }

        if !cert.node_id.is_empty() {
            self.node_id_index
                .insert(cert.node_id.clone(), cert.subject_public_key);
        }
        self.certificates.insert(cert.subject_public_key, cert);
        Ok(())
    }

    /// Add a certificate without validation (for loading pre-validated bundles).
    pub fn add_certificate_unchecked(&mut self, cert: MeshCertificate) {
        if !cert.node_id.is_empty() {
            self.node_id_index
                .insert(cert.node_id.clone(), cert.subject_public_key);
        }
        self.certificates.insert(cert.subject_public_key, cert);
    }

    /// Validate a peer's public key against the bundle.
    ///
    /// Returns `true` if the peer has a valid, non-expired certificate
    /// signed by a trusted authority.
    pub fn validate_peer(&self, peer_public_key: &[u8; 32], now_ms: u64) -> bool {
        match self.certificates.get(peer_public_key) {
            Some(cert) => {
                if !cert.is_valid(now_ms) {
                    debug!(
                        peer = hex::encode(peer_public_key),
                        "peer certificate expired"
                    );
                    return false;
                }
                if cert.verify().is_err() {
                    warn!(
                        peer = hex::encode(peer_public_key),
                        "peer certificate signature invalid"
                    );
                    return false;
                }
                true
            }
            None => false,
        }
    }

    /// Get the tier for a peer, if they have a valid certificate.
    pub fn get_peer_tier(&self, peer_public_key: &[u8; 32]) -> Option<MeshTier> {
        self.certificates.get(peer_public_key).map(|c| c.tier)
    }

    /// Get the permissions for a peer, if they have a valid certificate.
    pub fn get_peer_permissions(&self, peer_public_key: &[u8; 32]) -> Option<u8> {
        self.certificates
            .get(peer_public_key)
            .map(|c| c.permissions)
    }

    /// Get a certificate by subject public key.
    pub fn get_certificate(&self, public_key: &[u8; 32]) -> Option<&MeshCertificate> {
        self.certificates.get(public_key)
    }

    /// Get a certificate by node_id (discovery hostname).
    pub fn get_certificate_by_node_id(&self, node_id: &str) -> Option<&MeshCertificate> {
        self.node_id_index
            .get(node_id)
            .and_then(|pk| self.certificates.get(pk))
    }

    /// Validate a peer by node_id (discovery hostname).
    ///
    /// Returns `true` if the node_id maps to a valid, non-expired certificate.
    /// This is the primary validation entry point for PeerConnector, which
    /// discovers peers by hostname.
    pub fn validate_node_id(&self, node_id: &str, now_ms: u64) -> bool {
        match self.get_certificate_by_node_id(node_id) {
            Some(cert) => {
                if !cert.is_valid(now_ms) {
                    debug!(node_id, "peer certificate expired");
                    return false;
                }
                if cert.verify().is_err() {
                    warn!(node_id, "peer certificate signature invalid");
                    return false;
                }
                true
            }
            None => false,
        }
    }

    /// Get the tier for a node_id, if it has a valid certificate.
    pub fn get_node_tier(&self, node_id: &str) -> Option<MeshTier> {
        self.get_certificate_by_node_id(node_id).map(|c| c.tier)
    }

    /// Number of certificates in the bundle.
    pub fn len(&self) -> usize {
        self.certificates.len()
    }

    /// Whether the bundle is empty.
    pub fn is_empty(&self) -> bool {
        self.certificates.is_empty()
    }

    /// Number of trusted authorities.
    pub fn authority_count(&self) -> usize {
        self.authorities.len()
    }

    /// Remove expired certificates. Returns the number removed.
    pub fn remove_expired(&mut self, now_ms: u64) -> usize {
        let before = self.certificates.len();
        self.certificates.retain(|_, cert| {
            let valid = cert.is_valid(now_ms);
            if !valid && !cert.node_id.is_empty() {
                self.node_id_index.remove(&cert.node_id);
            }
            valid
        });
        before - self.certificates.len()
    }

    /// Load authority public keys from a directory.
    ///
    /// Each file should contain a raw 32-byte Ed25519 public key.
    pub fn load_authorities_from_dir(&mut self, dir: &Path) -> Result<usize, SecurityError> {
        let mut count = 0;
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let bytes = std::fs::read(&path)?;
                if bytes.len() == 32 {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&bytes);
                    // Validate it's a valid Ed25519 public key
                    if VerifyingKey::from_bytes(&key).is_ok() {
                        self.add_authority(key);
                        count += 1;
                        debug!(path = ?path, "loaded authority key");
                    } else {
                        warn!(path = ?path, "invalid Ed25519 public key, skipping");
                    }
                } else {
                    warn!(path = ?path, len = bytes.len(), "expected 32-byte key, skipping");
                }
            }
        }
        Ok(count)
    }

    /// Load certificates from a directory.
    ///
    /// Each file should contain a wire-encoded `MeshCertificate`.
    /// Certificates with unknown issuers are skipped (warning logged).
    pub fn load_certificates_from_dir(&mut self, dir: &Path) -> Result<usize, SecurityError> {
        let mut count = 0;
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let bytes = std::fs::read(&path)?;
                match MeshCertificate::decode(&bytes) {
                    Ok(cert) => match self.add_certificate(cert) {
                        Ok(()) => {
                            count += 1;
                            debug!(path = ?path, "loaded certificate");
                        }
                        Err(e) => {
                            warn!(path = ?path, error = %e, "certificate rejected");
                        }
                    },
                    Err(e) => {
                        warn!(path = ?path, error = %e, "failed to decode certificate");
                    }
                }
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn one_hour_ms() -> u64 {
        60 * 60 * 1000
    }

    #[test]
    fn test_mesh_tier_roundtrip() {
        for tier in [
            MeshTier::Enterprise,
            MeshTier::Regional,
            MeshTier::Tactical,
            MeshTier::Edge,
        ] {
            assert_eq!(MeshTier::from_byte(tier.to_byte()), Some(tier));
            assert_eq!(MeshTier::from_str_name(tier.as_str()), Some(tier));
        }
        assert_eq!(MeshTier::from_byte(99), None);
        assert_eq!(MeshTier::from_str_name("unknown"), None);
    }

    #[test]
    fn test_mesh_tier_case_insensitive() {
        assert_eq!(
            MeshTier::from_str_name("enterprise"),
            Some(MeshTier::Enterprise)
        );
        assert_eq!(
            MeshTier::from_str_name("TACTICAL"),
            Some(MeshTier::Tactical)
        );
        assert_eq!(MeshTier::from_str_name(" Edge "), Some(MeshTier::Edge));
    }

    #[test]
    fn test_mesh_tier_ordering() {
        assert!(MeshTier::Enterprise < MeshTier::Regional);
        assert!(MeshTier::Regional < MeshTier::Tactical);
        assert!(MeshTier::Tactical < MeshTier::Edge);
    }

    /// Helper: create a test cert with node_id.
    fn make_cert(
        authority: &DeviceKeypair,
        member: &DeviceKeypair,
        node_id: &str,
        tier: MeshTier,
        perms: u8,
        issued: u64,
        expires: u64,
    ) -> MeshCertificate {
        MeshCertificate::new(
            member.public_key_bytes(),
            "DEADBEEF".to_string(),
            node_id.to_string(),
            tier,
            perms,
            issued,
            expires,
            authority.public_key_bytes(),
        )
        .signed(authority)
    }

    #[test]
    fn test_certificate_sign_verify() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let cert = make_cert(
            &authority,
            &member,
            "tac-1",
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            now + one_hour_ms(),
        );

        assert!(cert.verify().is_ok());
        assert!(cert.is_valid(now));
        assert!(!cert.is_root());
        assert!(cert.has_permission(permissions::RELAY));
        assert!(cert.has_permission(permissions::EMERGENCY));
        assert!(!cert.has_permission(permissions::ADMIN));
        assert_eq!(cert.node_id, "tac-1");
    }

    #[test]
    fn test_certificate_root() {
        let authority = DeviceKeypair::generate();
        let now = now_ms();

        let cert = MeshCertificate::new_root(
            &authority,
            "DEADBEEF".to_string(),
            "enterprise-0".to_string(),
            MeshTier::Enterprise,
            now,
            now + one_hour_ms(),
        );

        assert!(cert.verify().is_ok());
        assert!(cert.is_root());
        assert!(cert.has_permission(permissions::AUTHORITY));
        assert_eq!(cert.node_id, "enterprise-0");
    }

    #[test]
    fn test_certificate_expired() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let cert = make_cert(
            &authority,
            &member,
            "tac-1",
            MeshTier::Tactical,
            permissions::STANDARD,
            now - 2 * one_hour_ms(),
            now - one_hour_ms(),
        );

        assert!(cert.verify().is_ok());
        assert!(!cert.is_valid(now));
    }

    #[test]
    fn test_certificate_no_expiration() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let cert = make_cert(
            &authority,
            &member,
            "tac-1",
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            0,
        );

        assert!(cert.is_valid(now));
        assert!(cert.is_valid(now + 365 * 24 * one_hour_ms()));
        assert_eq!(cert.time_remaining_ms(now), u64::MAX);
    }

    #[test]
    fn test_certificate_wrong_signer() {
        let authority = DeviceKeypair::generate();
        let imposter = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        // Claims authority issued it, but imposter signed it
        let cert = MeshCertificate::new(
            member.public_key_bytes(),
            "DEADBEEF".to_string(),
            "tac-1".to_string(),
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            now + one_hour_ms(),
            authority.public_key_bytes(),
        )
        .signed(&imposter);

        assert!(cert.verify().is_err());
    }

    #[test]
    fn test_certificate_encode_decode() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let cert = MeshCertificate::new(
            member.public_key_bytes(),
            "A1B2C3D4".to_string(),
            "regional-hub-1".to_string(),
            MeshTier::Regional,
            permissions::STANDARD | permissions::ENROLL,
            now,
            now + one_hour_ms(),
            authority.public_key_bytes(),
        )
        .signed(&authority);

        let encoded = cert.encode();
        let decoded = MeshCertificate::decode(&encoded).unwrap();

        assert_eq!(decoded.subject_public_key, cert.subject_public_key);
        assert_eq!(decoded.mesh_id, cert.mesh_id);
        assert_eq!(decoded.node_id, "regional-hub-1");
        assert_eq!(decoded.tier, cert.tier);
        assert_eq!(decoded.permissions, cert.permissions);
        assert_eq!(decoded.issued_at_ms, cert.issued_at_ms);
        assert_eq!(decoded.expires_at_ms, cert.expires_at_ms);
        assert_eq!(decoded.issuer_public_key, cert.issuer_public_key);
        assert_eq!(decoded.signature, cert.signature);
        assert!(decoded.verify().is_ok());
    }

    #[test]
    fn test_certificate_decode_too_short() {
        assert!(MeshCertificate::decode(&[0u8; 10]).is_err());
    }

    #[test]
    fn test_bundle_validate_peer() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let cert = make_cert(
            &authority,
            &member,
            "tac-1",
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            now + one_hour_ms(),
        );

        let mut bundle = CertificateBundle::new();
        bundle.add_authority(authority.public_key_bytes());
        bundle.add_certificate(cert).unwrap();

        assert!(bundle.validate_peer(&member.public_key_bytes(), now));
        assert_eq!(
            bundle.get_peer_tier(&member.public_key_bytes()),
            Some(MeshTier::Tactical)
        );
        assert_eq!(
            bundle.get_peer_permissions(&member.public_key_bytes()),
            Some(permissions::STANDARD)
        );

        let stranger = DeviceKeypair::generate();
        assert!(!bundle.validate_peer(&stranger.public_key_bytes(), now));
    }

    #[test]
    fn test_bundle_validate_by_node_id() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let cert = make_cert(
            &authority,
            &member,
            "tactical-west-3",
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            now + one_hour_ms(),
        );

        let mut bundle = CertificateBundle::new();
        bundle.add_authority(authority.public_key_bytes());
        bundle.add_certificate(cert).unwrap();

        // Validate by node_id (the PeerConnector path)
        assert!(bundle.validate_node_id("tactical-west-3", now));
        assert!(!bundle.validate_node_id("unknown-node", now));

        // Tier lookup by node_id
        assert_eq!(
            bundle.get_node_tier("tactical-west-3"),
            Some(MeshTier::Tactical)
        );
        assert_eq!(bundle.get_node_tier("unknown"), None);

        // Certificate lookup by node_id
        let found = bundle
            .get_certificate_by_node_id("tactical-west-3")
            .unwrap();
        assert_eq!(found.subject_public_key, member.public_key_bytes());
    }

    #[test]
    fn test_bundle_rejects_untrusted_issuer() {
        let untrusted = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let cert = MeshCertificate::new(
            member.public_key_bytes(),
            "DEADBEEF".to_string(),
            "tac-1".to_string(),
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            now + one_hour_ms(),
            untrusted.public_key_bytes(),
        )
        .signed(&untrusted);

        let mut bundle = CertificateBundle::new();
        let result = bundle.add_certificate(cert);
        assert!(result.is_err());
    }

    #[test]
    fn test_bundle_accepts_root_cert() {
        let authority = DeviceKeypair::generate();
        let now = now_ms();

        let root = MeshCertificate::new_root(
            &authority,
            "DEADBEEF".to_string(),
            "enterprise-0".to_string(),
            MeshTier::Enterprise,
            now,
            now + one_hour_ms(),
        );

        let mut bundle = CertificateBundle::new();
        bundle.add_certificate(root).unwrap();
        assert!(bundle.validate_peer(&authority.public_key_bytes(), now));
        assert!(bundle.validate_node_id("enterprise-0", now));
    }

    #[test]
    fn test_bundle_remove_expired() {
        let authority = DeviceKeypair::generate();
        let now = now_ms();

        let expired_member = DeviceKeypair::generate();
        let expired_cert = make_cert(
            &authority,
            &expired_member,
            "expired-node",
            MeshTier::Tactical,
            permissions::STANDARD,
            now - 2 * one_hour_ms(),
            now - one_hour_ms(),
        );

        let valid_member = DeviceKeypair::generate();
        let valid_cert = make_cert(
            &authority,
            &valid_member,
            "valid-node",
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            now + one_hour_ms(),
        );

        let mut bundle = CertificateBundle::new();
        bundle.add_authority(authority.public_key_bytes());
        bundle.add_certificate_unchecked(expired_cert);
        bundle.add_certificate(valid_cert).unwrap();
        assert_eq!(bundle.len(), 2);

        let removed = bundle.remove_expired(now);
        assert_eq!(removed, 1);
        assert_eq!(bundle.len(), 1);
        // node_id index cleaned up
        assert!(!bundle.validate_node_id("expired-node", now));
        assert!(bundle.validate_node_id("valid-node", now));
    }

    #[test]
    fn test_bundle_load_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let authority = DeviceKeypair::generate();

        let auth_dir = dir.path().join("authorities");
        std::fs::create_dir(&auth_dir).unwrap();
        std::fs::write(auth_dir.join("root.key"), authority.public_key_bytes()).unwrap();

        let cert_dir = dir.path().join("certificates");
        std::fs::create_dir(&cert_dir).unwrap();

        let member = DeviceKeypair::generate();
        let now = now_ms();
        let cert = make_cert(
            &authority,
            &member,
            "tac-1",
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            now + one_hour_ms(),
        );

        std::fs::write(cert_dir.join("member.cert"), cert.encode()).unwrap();

        let mut bundle = CertificateBundle::new();
        let auth_count = bundle.load_authorities_from_dir(&auth_dir).unwrap();
        assert_eq!(auth_count, 1);

        let cert_count = bundle.load_certificates_from_dir(&cert_dir).unwrap();
        assert_eq!(cert_count, 1);

        assert!(bundle.validate_peer(&member.public_key_bytes(), now));
        assert!(bundle.validate_node_id("tac-1", now));
    }

    #[test]
    fn test_time_remaining() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let cert = make_cert(
            &authority,
            &member,
            "tac-1",
            MeshTier::Tactical,
            permissions::STANDARD,
            now,
            now + one_hour_ms(),
        );

        let remaining = cert.time_remaining_ms(now);
        assert!(remaining > 0);
        assert!(remaining <= one_hour_ms());

        let remaining_expired = cert.time_remaining_ms(now + 2 * one_hour_ms());
        assert_eq!(remaining_expired, 0);
    }
}
