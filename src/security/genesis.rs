//! Mesh genesis protocol for creating new Peat mesh formations.
//!
//! A mesh is created through a genesis event where:
//! - A 256-bit cryptographic seed is generated (CSPRNG)
//! - The mesh_id is derived from the name and seed
//! - The formation_secret is derived from the seed (used for Iroh EndpointId HKDF)
//! - The creator's keypair becomes the initial authority
//! - A self-signed root certificate is issued
//!
//! # Key Derivation
//!
//! All secrets are derived from the mesh_seed via HKDF-SHA256 with distinct
//! context strings, ensuring domain separation:
//!
//! ```text
//! mesh_seed (256-bit, CSPRNG)
//!     │
//!     ├── HKDF("peat-mesh:mesh-id") ──► mesh_id (first 4 bytes → 8 hex chars)
//!     ├── HKDF("peat-mesh:formation-secret") ──► formation_secret (32 bytes)
//!     └── HKDF("peat-mesh:authority-keypair") ──► authority Ed25519 keypair
//! ```
//!
//! # Example
//!
//! ```
//! use peat_mesh::security::{MeshGenesis, MembershipPolicy, DeviceKeypair};
//!
//! // Create a new mesh (authority keypair derived from seed)
//! let genesis = MeshGenesis::create("ALPHA-TEAM", MembershipPolicy::Controlled);
//!
//! // Get derived values
//! let mesh_id = genesis.mesh_id();             // e.g., "A1B2C3D4"
//! let formation_secret = genesis.formation_secret();  // 32 bytes
//! let authority = genesis.authority();          // DeviceKeypair
//! let root_cert = genesis.root_certificate();  // self-signed MeshCertificate
//! ```

use rand_core::{OsRng, RngCore};

use super::certificate::{MeshCertificate, MeshTier};
use super::error::SecurityError;
use super::keypair::DeviceKeypair;

/// Membership policy controlling how nodes can join the mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MembershipPolicy {
    /// Anyone with formation_secret can join. Least secure.
    Open,

    /// Explicit enrollment by an authority is required. Default.
    #[default]
    Controlled,

    /// Only pre-provisioned devices can join. Highest security.
    Strict,
}

impl MembershipPolicy {
    /// Encode as a single byte.
    pub fn to_byte(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Controlled => 1,
            Self::Strict => 2,
        }
    }

    /// Decode from a single byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Open),
            1 => Some(Self::Controlled),
            2 => Some(Self::Strict),
            _ => None,
        }
    }

    /// Parse from a string (case-insensitive).
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "open" => Some(Self::Open),
            "controlled" => Some(Self::Controlled),
            "strict" => Some(Self::Strict),
            _ => None,
        }
    }

    /// Get the policy name as a string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Controlled => "Controlled",
            Self::Strict => "Strict",
        }
    }
}

impl std::fmt::Display for MembershipPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Genesis event for creating a new mesh formation.
///
/// Contains all cryptographic material needed to bootstrap a mesh from zero.
/// The genesis artifact is the root of trust — the authority keypair signs
/// all certificates and the formation_secret authenticates transport.
///
/// # Security
///
/// The `mesh_seed` is the root secret. Protect it carefully:
/// - Store encrypted at rest
/// - Never transmit over the network
/// - Only the genesis creator needs it (for recovery)
///
/// Shareable credentials (via [`MeshCredentials`]) exclude the seed and
/// authority private key.
#[derive(Clone)]
pub struct MeshGenesis {
    /// Human-readable mesh name.
    pub mesh_name: String,

    /// 256-bit cryptographic seed (generated from CSPRNG).
    mesh_seed: [u8; 32],

    /// Authority Ed25519 keypair (derived from mesh_seed).
    authority: DeviceKeypair,

    /// Timestamp of creation (milliseconds since Unix epoch).
    pub created_at_ms: u64,

    /// Membership policy for this mesh.
    pub policy: MembershipPolicy,
}

impl MeshGenesis {
    /// HKDF context for mesh_id derivation.
    const MESH_ID_CONTEXT: &'static str = "peat-mesh:mesh-id";

    /// HKDF context for formation secret derivation.
    const FORMATION_SECRET_CONTEXT: &'static str = "peat-mesh:formation-secret";

    /// HKDF context for authority keypair derivation.
    const AUTHORITY_CONTEXT: &'static str = "peat-mesh:authority-keypair";

    /// Create a new mesh formation with a random seed.
    ///
    /// The authority keypair is deterministically derived from the seed.
    pub fn create(mesh_name: &str, policy: MembershipPolicy) -> Self {
        let mut mesh_seed = [0u8; 32];
        OsRng.fill_bytes(&mut mesh_seed);
        Self::with_seed(mesh_name, mesh_seed, policy)
    }

    /// Create a genesis with a specific seed (for testing or deterministic creation).
    ///
    /// # Safety
    ///
    /// Only use with cryptographically random seeds in production.
    pub fn with_seed(mesh_name: &str, mesh_seed: [u8; 32], policy: MembershipPolicy) -> Self {
        let authority =
            DeviceKeypair::from_seed(&mesh_seed, Self::AUTHORITY_CONTEXT).expect("HKDF infallible");
        Self {
            mesh_name: mesh_name.into(),
            mesh_seed,
            authority,
            created_at_ms: now_ms(),
            policy,
        }
    }

    /// Create a genesis with a specific seed and an externally-provided authority keypair.
    ///
    /// Use when the authority keypair is generated independently (e.g., from a
    /// hardware security module) rather than derived from the seed.
    pub fn with_authority(
        mesh_name: &str,
        mesh_seed: [u8; 32],
        authority: DeviceKeypair,
        policy: MembershipPolicy,
    ) -> Self {
        Self {
            mesh_name: mesh_name.into(),
            mesh_seed,
            authority,
            created_at_ms: now_ms(),
            policy,
        }
    }

    /// Derive the mesh_id from name and seed.
    ///
    /// The mesh_id is 8 hex characters derived from HKDF-SHA256.
    /// Format: uppercase hex, e.g., "A1B2C3D4".
    pub fn mesh_id(&self) -> String {
        let hash = self.derive(Self::MESH_ID_CONTEXT);
        format!(
            "{:02X}{:02X}{:02X}{:02X}",
            hash[0], hash[1], hash[2], hash[3]
        )
    }

    /// Derive the formation secret.
    ///
    /// The formation secret is shared with all mesh members and used for
    /// HKDF-based Iroh EndpointId derivation:
    ///
    /// ```text
    /// HKDF(formation_secret, "iroh:" + node_id) → EndpointId
    /// ```
    pub fn formation_secret(&self) -> [u8; 32] {
        self.derive(Self::FORMATION_SECRET_CONTEXT)
    }

    /// Get the authority keypair.
    pub fn authority(&self) -> &DeviceKeypair {
        &self.authority
    }

    /// Get the authority's public key bytes.
    pub fn authority_public_key(&self) -> [u8; 32] {
        self.authority.public_key_bytes()
    }

    /// Get the mesh seed for secure storage.
    ///
    /// **Security**: This is the root secret. Protect it carefully.
    pub fn mesh_seed(&self) -> &[u8; 32] {
        &self.mesh_seed
    }

    /// Generate a self-signed root certificate for the authority node.
    ///
    /// The root cert identifies the genesis authority in the mesh:
    /// - `subject_public_key` = `issuer_public_key` (self-signed)
    /// - `tier` = Enterprise (highest trust)
    /// - `permissions` = AUTHORITY (all permissions)
    /// - `expires_at_ms` = 0 (no expiration, root cert is permanent)
    pub fn root_certificate(&self, node_id: &str) -> MeshCertificate {
        let now = now_ms();
        MeshCertificate::new_root(
            &self.authority,
            self.mesh_id(),
            node_id.to_string(),
            MeshTier::Enterprise,
            now,
            0, // No expiration for root cert
        )
    }

    /// Issue a signed certificate for a new member.
    ///
    /// This is a convenience method for the genesis authority to enroll a node.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_certificate(
        &self,
        subject_public_key: [u8; 32],
        node_id: &str,
        tier: MeshTier,
        permissions: u8,
        validity_ms: u64,
    ) -> MeshCertificate {
        let now = now_ms();
        let expires = if validity_ms == 0 {
            0
        } else {
            now + validity_ms
        };
        MeshCertificate::new(
            subject_public_key,
            self.mesh_id(),
            node_id.to_string(),
            tier,
            permissions,
            now,
            expires,
            self.authority.public_key_bytes(),
        )
        .signed(&self.authority)
    }

    /// Build shareable credentials (no seed, no authority private key).
    pub fn credentials(&self) -> MeshCredentials {
        MeshCredentials {
            mesh_id: self.mesh_id(),
            mesh_name: self.mesh_name.clone(),
            formation_secret: self.formation_secret(),
            authority_public_key: self.authority_public_key(),
            policy: self.policy,
        }
    }

    /// Encode genesis data for secure persistence.
    ///
    /// Format:
    /// - mesh_name length (2 bytes, LE)
    /// - mesh_name (variable)
    /// - mesh_seed (32 bytes)
    /// - authority secret key (32 bytes) — SENSITIVE!
    /// - created_at_ms (8 bytes, LE)
    /// - policy (1 byte)
    ///
    /// Total: 75 + mesh_name.len() bytes
    pub fn encode(&self) -> Vec<u8> {
        let name_bytes = self.mesh_name.as_bytes();
        let mut buf = Vec::with_capacity(75 + name_bytes.len());

        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&self.mesh_seed);
        buf.extend_from_slice(&self.authority.secret_key_bytes());
        buf.extend_from_slice(&self.created_at_ms.to_le_bytes());
        buf.push(self.policy.to_byte());

        buf
    }

    /// Decode genesis data from bytes.
    pub fn decode(data: &[u8]) -> Result<Self, SecurityError> {
        // Minimum: 2 + 0 + 32 + 32 + 8 + 1 = 75
        if data.len() < 75 {
            return Err(SecurityError::SerializationError(format!(
                "genesis too short: {} bytes (min 75)",
                data.len()
            )));
        }

        let name_len = u16::from_le_bytes([data[0], data[1]]) as usize;
        if data.len() < 75 + name_len {
            return Err(SecurityError::SerializationError(
                "genesis truncated at mesh_name".to_string(),
            ));
        }

        let mesh_name = String::from_utf8(data[2..2 + name_len].to_vec())
            .map_err(|e| SecurityError::SerializationError(format!("invalid mesh_name: {e}")))?;
        let offset = 2 + name_len;

        let mut mesh_seed = [0u8; 32];
        mesh_seed.copy_from_slice(&data[offset..offset + 32]);

        let authority = DeviceKeypair::from_secret_bytes(&data[offset + 32..offset + 64])?;

        let created_at_ms = u64::from_le_bytes(data[offset + 64..offset + 72].try_into().unwrap());

        let policy = MembershipPolicy::from_byte(data[offset + 72])
            .ok_or_else(|| SecurityError::SerializationError("invalid policy byte".to_string()))?;

        Ok(Self {
            mesh_name,
            mesh_seed,
            authority,
            created_at_ms,
            policy,
        })
    }

    /// Derive 32 bytes from the mesh_seed with a context string.
    fn derive(&self, context: &str) -> [u8; 32] {
        use hkdf::Hkdf;
        use sha2::Sha256;

        let hk = Hkdf::<Sha256>::new(Some(self.mesh_name.as_bytes()), &self.mesh_seed);
        let mut okm = [0u8; 32];
        hk.expand(context.as_bytes(), &mut okm)
            .expect("32-byte output is within HKDF limit");
        okm
    }
}

impl std::fmt::Debug for MeshGenesis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshGenesis")
            .field("mesh_name", &self.mesh_name)
            .field("mesh_id", &self.mesh_id())
            .field("authority_device_id", &self.authority.device_id())
            .field("created_at_ms", &self.created_at_ms)
            .field("policy", &self.policy)
            .field("mesh_seed", &"[REDACTED]")
            .finish()
    }
}

/// Shareable mesh credentials (no seed, no authority private key).
///
/// This can be distributed to nodes joining the mesh. It includes everything
/// needed for transport (formation_secret) and certificate validation
/// (authority_public_key), but NOT the ability to issue certificates.
#[derive(Debug, Clone)]
pub struct MeshCredentials {
    /// The mesh_id (derived, for verification).
    pub mesh_id: String,

    /// Mesh name.
    pub mesh_name: String,

    /// Formation secret for HKDF-based Iroh EndpointId derivation.
    pub formation_secret: [u8; 32],

    /// Authority's public key (for certificate verification).
    pub authority_public_key: [u8; 32],

    /// Membership policy.
    pub policy: MembershipPolicy,
}

impl MeshCredentials {
    /// Encode for distribution (e.g., QR code, config file).
    ///
    /// Format:
    /// - mesh_name length (2 bytes, LE)
    /// - mesh_name (variable)
    /// - mesh_id (8 bytes, ASCII hex)
    /// - formation_secret (32 bytes)
    /// - authority_public_key (32 bytes)
    /// - policy (1 byte)
    ///
    /// Total: 75 + mesh_name.len() bytes
    pub fn encode(&self) -> Vec<u8> {
        let name_bytes = self.mesh_name.as_bytes();
        let mesh_id_bytes = self.mesh_id.as_bytes();
        let mut buf = Vec::with_capacity(75 + name_bytes.len());

        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(mesh_id_bytes);
        buf.extend_from_slice(&self.formation_secret);
        buf.extend_from_slice(&self.authority_public_key);
        buf.push(self.policy.to_byte());

        buf
    }

    /// Decode from bytes.
    pub fn decode(data: &[u8]) -> Result<Self, SecurityError> {
        // Minimum: 2 + 0 + 8 + 32 + 32 + 1 = 75
        if data.len() < 75 {
            return Err(SecurityError::SerializationError(format!(
                "credentials too short: {} bytes (min 75)",
                data.len()
            )));
        }

        let name_len = u16::from_le_bytes([data[0], data[1]]) as usize;
        if data.len() < 75 + name_len {
            return Err(SecurityError::SerializationError(
                "credentials truncated at mesh_name".to_string(),
            ));
        }

        let mesh_name = String::from_utf8(data[2..2 + name_len].to_vec())
            .map_err(|e| SecurityError::SerializationError(format!("invalid mesh_name: {e}")))?;
        let offset = 2 + name_len;

        let mesh_id = String::from_utf8(data[offset..offset + 8].to_vec())
            .map_err(|e| SecurityError::SerializationError(format!("invalid mesh_id: {e}")))?;

        let mut formation_secret = [0u8; 32];
        formation_secret.copy_from_slice(&data[offset + 8..offset + 40]);

        let mut authority_public_key = [0u8; 32];
        authority_public_key.copy_from_slice(&data[offset + 40..offset + 72]);

        let policy = MembershipPolicy::from_byte(data[offset + 72])
            .ok_or_else(|| SecurityError::SerializationError("invalid policy byte".to_string()))?;

        Ok(Self {
            mesh_id,
            mesh_name,
            formation_secret,
            authority_public_key,
            policy,
        })
    }
}

/// Get current timestamp in milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::certificate::permissions;
    use super::*;

    #[test]
    fn test_create_genesis() {
        let genesis = MeshGenesis::create("ALPHA-TEAM", MembershipPolicy::Controlled);

        assert_eq!(genesis.mesh_name, "ALPHA-TEAM");
        assert_eq!(genesis.policy, MembershipPolicy::Controlled);
        assert!(genesis.created_at_ms > 0);
    }

    #[test]
    fn test_mesh_id_format() {
        let genesis = MeshGenesis::create("TEST", MembershipPolicy::Open);
        let mesh_id = genesis.mesh_id();

        assert_eq!(mesh_id.len(), 8);
        assert!(mesh_id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_lowercase()));
    }

    #[test]
    fn test_mesh_id_deterministic() {
        let seed = [0x42u8; 32];
        let genesis = MeshGenesis::with_seed("TEST", seed, MembershipPolicy::Open);

        assert_eq!(genesis.mesh_id(), genesis.mesh_id());
    }

    #[test]
    fn test_different_names_different_ids() {
        let seed = [0x42u8; 32];
        let g1 = MeshGenesis::with_seed("ALPHA", seed, MembershipPolicy::Open);
        let g2 = MeshGenesis::with_seed("BRAVO", seed, MembershipPolicy::Open);

        assert_ne!(g1.mesh_id(), g2.mesh_id());
    }

    #[test]
    fn test_different_seeds_different_ids() {
        let g1 = MeshGenesis::with_seed("TEST", [0x42u8; 32], MembershipPolicy::Open);
        let g2 = MeshGenesis::with_seed("TEST", [0x43u8; 32], MembershipPolicy::Open);

        assert_ne!(g1.mesh_id(), g2.mesh_id());
    }

    #[test]
    fn test_formation_secret_deterministic() {
        let seed = [0x42u8; 32];
        let genesis = MeshGenesis::with_seed("TEST", seed, MembershipPolicy::Open);

        let s1 = genesis.formation_secret();
        let s2 = genesis.formation_secret();

        assert_eq!(s1, s2);
        assert_ne!(s1, seed); // Derived, not the seed itself
    }

    #[test]
    fn test_formation_secret_differs_from_mesh_id_source() {
        let genesis = MeshGenesis::create("TEST", MembershipPolicy::Open);
        let formation = genesis.formation_secret();
        let mesh_id_bytes = genesis.derive(MeshGenesis::MESH_ID_CONTEXT);

        assert_ne!(formation, mesh_id_bytes); // Different HKDF contexts
    }

    #[test]
    fn test_authority_keypair_deterministic() {
        let seed = [0x42u8; 32];
        let g1 = MeshGenesis::with_seed("TEST", seed, MembershipPolicy::Open);
        let g2 = MeshGenesis::with_seed("TEST", seed, MembershipPolicy::Open);

        assert_eq!(g1.authority_public_key(), g2.authority_public_key());
    }

    #[test]
    fn test_authority_can_sign_and_verify() {
        let genesis = MeshGenesis::create("TEST", MembershipPolicy::Open);
        let msg = b"hello mesh";
        let sig = genesis.authority().sign(msg);
        assert!(genesis.authority().verify(msg, &sig).is_ok());
    }

    #[test]
    fn test_root_certificate() {
        let genesis = MeshGenesis::create("TEST", MembershipPolicy::Controlled);
        let root = genesis.root_certificate("enterprise-0");

        assert!(root.verify().is_ok());
        assert!(root.is_root());
        assert_eq!(root.mesh_id, genesis.mesh_id());
        assert_eq!(root.node_id, "enterprise-0");
        assert_eq!(root.tier, MeshTier::Enterprise);
        assert_eq!(root.permissions, permissions::AUTHORITY);
        assert_eq!(root.expires_at_ms, 0); // No expiration
        assert_eq!(root.subject_public_key, genesis.authority_public_key());
        assert_eq!(root.issuer_public_key, genesis.authority_public_key());
    }

    #[test]
    fn test_issue_certificate() {
        let genesis = MeshGenesis::create("TEST", MembershipPolicy::Controlled);
        let member = DeviceKeypair::generate();

        let cert = genesis.issue_certificate(
            member.public_key_bytes(),
            "tac-west-1",
            MeshTier::Tactical,
            permissions::STANDARD,
            24 * 60 * 60 * 1000, // 24 hours
        );

        assert!(cert.verify().is_ok());
        assert!(!cert.is_root());
        assert_eq!(cert.mesh_id, genesis.mesh_id());
        assert_eq!(cert.node_id, "tac-west-1");
        assert_eq!(cert.tier, MeshTier::Tactical);
        assert_eq!(cert.permissions, permissions::STANDARD);
        assert_eq!(cert.subject_public_key, member.public_key_bytes());
        assert_eq!(cert.issuer_public_key, genesis.authority_public_key());
        assert!(cert.expires_at_ms > cert.issued_at_ms);
    }

    #[test]
    fn test_issue_certificate_no_expiration() {
        let genesis = MeshGenesis::create("TEST", MembershipPolicy::Open);
        let member = DeviceKeypair::generate();

        let cert = genesis.issue_certificate(
            member.public_key_bytes(),
            "hub-1",
            MeshTier::Regional,
            permissions::STANDARD | permissions::ENROLL,
            0, // No expiration
        );

        assert!(cert.verify().is_ok());
        assert_eq!(cert.expires_at_ms, 0);
    }

    #[test]
    fn test_credentials() {
        let genesis = MeshGenesis::create("TEST", MembershipPolicy::Controlled);
        let creds = genesis.credentials();

        assert_eq!(creds.mesh_id, genesis.mesh_id());
        assert_eq!(creds.mesh_name, genesis.mesh_name);
        assert_eq!(creds.formation_secret, genesis.formation_secret());
        assert_eq!(creds.authority_public_key, genesis.authority_public_key());
        assert_eq!(creds.policy, genesis.policy);
    }

    #[test]
    fn test_encode_decode_genesis_roundtrip() {
        let genesis = MeshGenesis::create("ALPHA-TEAM", MembershipPolicy::Strict);
        let encoded = genesis.encode();
        let decoded = MeshGenesis::decode(&encoded).unwrap();

        assert_eq!(decoded.mesh_name, genesis.mesh_name);
        assert_eq!(decoded.mesh_id(), genesis.mesh_id());
        assert_eq!(decoded.formation_secret(), genesis.formation_secret());
        assert_eq!(
            decoded.authority_public_key(),
            genesis.authority_public_key()
        );
        assert_eq!(decoded.policy, genesis.policy);
    }

    #[test]
    fn test_decode_genesis_too_short() {
        assert!(MeshGenesis::decode(&[0u8; 10]).is_err());
    }

    #[test]
    fn test_decode_genesis_invalid_policy() {
        let genesis = MeshGenesis::create("X", MembershipPolicy::Open);
        let mut encoded = genesis.encode();
        // Corrupt the policy byte (last byte)
        *encoded.last_mut().unwrap() = 99;
        assert!(MeshGenesis::decode(&encoded).is_err());
    }

    #[test]
    fn test_encode_decode_credentials_roundtrip() {
        let genesis = MeshGenesis::create("BRAVO-NET", MembershipPolicy::Controlled);
        let creds = genesis.credentials();
        let encoded = creds.encode();
        let decoded = MeshCredentials::decode(&encoded).unwrap();

        assert_eq!(decoded.mesh_id, creds.mesh_id);
        assert_eq!(decoded.mesh_name, creds.mesh_name);
        assert_eq!(decoded.formation_secret, creds.formation_secret);
        assert_eq!(decoded.authority_public_key, creds.authority_public_key);
        assert_eq!(decoded.policy, creds.policy);
    }

    #[test]
    fn test_decode_credentials_too_short() {
        assert!(MeshCredentials::decode(&[0u8; 10]).is_err());
    }

    #[test]
    fn test_with_authority_external_keypair() {
        let external_authority = DeviceKeypair::generate();
        let seed = [0x42u8; 32];
        let genesis = MeshGenesis::with_authority(
            "HSM-MESH",
            seed,
            external_authority.clone(),
            MembershipPolicy::Strict,
        );

        assert_eq!(
            genesis.authority_public_key(),
            external_authority.public_key_bytes()
        );
        // External authority ≠ seed-derived authority
        let derived = MeshGenesis::with_seed("HSM-MESH", seed, MembershipPolicy::Strict);
        assert_ne!(
            genesis.authority_public_key(),
            derived.authority_public_key()
        );
    }

    #[test]
    fn test_policy_default() {
        assert_eq!(MembershipPolicy::default(), MembershipPolicy::Controlled);
    }

    #[test]
    fn test_policy_from_str_name() {
        assert_eq!(
            MembershipPolicy::from_str_name("open"),
            Some(MembershipPolicy::Open)
        );
        assert_eq!(
            MembershipPolicy::from_str_name("CONTROLLED"),
            Some(MembershipPolicy::Controlled)
        );
        assert_eq!(
            MembershipPolicy::from_str_name(" Strict "),
            Some(MembershipPolicy::Strict)
        );
        assert_eq!(MembershipPolicy::from_str_name("invalid"), None);
    }

    #[test]
    fn test_policy_byte_roundtrip() {
        for policy in [
            MembershipPolicy::Open,
            MembershipPolicy::Controlled,
            MembershipPolicy::Strict,
        ] {
            assert_eq!(MembershipPolicy::from_byte(policy.to_byte()), Some(policy));
        }
        assert_eq!(MembershipPolicy::from_byte(99), None);
    }

    #[test]
    fn test_debug_redacts_seed() {
        let genesis = MeshGenesis::create("TEST", MembershipPolicy::Open);
        let debug_str = format!("{:?}", genesis);
        assert!(debug_str.contains("REDACTED"));
        assert!(debug_str.contains("mesh_id"));
        // Seed value should not appear (only the field name with [REDACTED])
        let seed_hex = hex::encode(genesis.mesh_seed());
        assert!(!debug_str.contains(&seed_hex));
    }
}
