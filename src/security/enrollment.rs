//! Enrollment protocol types for mesh membership.
//!
//! Defines the wire types and service trait for enrolling new nodes into a
//! mesh formation. The enrollment flow:
//!
//! ```text
//! New Node                            Authority (or Enrollment Service)
//!     |                                          |
//!     |  ── EnrollmentRequest ──────────────►    |
//!     |     (public key + bootstrap token)       |
//!     |                                          |
//!     |  ◄── EnrollmentResponse ──────────────   |
//!     |     (status + signed certificate)        |
//!     |                                          |
//! ```
//!
//! Bootstrap tokens are opaque secrets distributed out-of-band (QR code, BLE,
//! pre-provisioned config). The authority validates the token and issues a
//! [`MeshCertificate`](super::certificate::MeshCertificate) on approval.

use ed25519_dalek::Verifier;

use super::certificate::{MeshCertificate, MeshTier};
use super::error::SecurityError;
use super::keypair::DeviceKeypair;

/// Status of an enrollment request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnrollmentStatus {
    /// Request received and awaiting authority decision.
    Pending,
    /// Enrollment approved — certificate is attached.
    Approved,
    /// Enrollment denied.
    Denied { reason: String },
    /// Previously approved enrollment has been revoked.
    Revoked { reason: String },
}

impl EnrollmentStatus {
    /// Encode to a single status byte + optional reason.
    pub fn to_byte(&self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::Approved => 1,
            Self::Denied { .. } => 2,
            Self::Revoked { .. } => 3,
        }
    }
}

/// A request from a node to join a mesh formation.
///
/// Wire format:
/// ```text
/// [subject_pubkey:32][mesh_id_len:1][mesh_id:N][node_id_len:1][node_id:P]
/// [requested_tier:1][token_len:2 LE][token:M][timestamp:8 LE][signature:64]
/// ```
#[derive(Clone, Debug)]
pub struct EnrollmentRequest {
    /// The requesting node's Ed25519 public key.
    pub subject_public_key: [u8; 32],
    /// The mesh/formation ID to join.
    pub mesh_id: String,
    /// The requesting node's identifier (hostname).
    pub node_id: String,
    /// Requested tier (authority may override).
    pub requested_tier: MeshTier,
    /// Out-of-band bootstrap token proving authorization to join.
    pub bootstrap_token: Vec<u8>,
    /// Request timestamp (Unix epoch milliseconds).
    pub timestamp_ms: u64,
    /// Ed25519 signature over the signable portion (proves key ownership).
    pub signature: [u8; 64],
}

impl EnrollmentRequest {
    /// Create a new enrollment request.
    pub fn new(
        keypair: &DeviceKeypair,
        mesh_id: String,
        node_id: String,
        requested_tier: MeshTier,
        bootstrap_token: Vec<u8>,
        timestamp_ms: u64,
    ) -> Self {
        let mut req = Self {
            subject_public_key: keypair.public_key_bytes(),
            mesh_id,
            node_id,
            requested_tier,
            bootstrap_token,
            timestamp_ms,
            signature: [0u8; 64],
        };
        let signable = req.signable_bytes();
        req.signature = keypair.sign(&signable).to_bytes();
        req
    }

    /// Verify the request signature (proves the requester owns the private key).
    pub fn verify_signature(&self) -> Result<(), SecurityError> {
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&self.subject_public_key)
            .map_err(|e| SecurityError::InvalidPublicKey(e.to_string()))?;
        let sig = ed25519_dalek::Signature::from_bytes(&self.signature);
        let signable = self.signable_bytes();
        vk.verify(&signable, &sig)
            .map_err(|e| SecurityError::InvalidSignature(e.to_string()))
    }

    fn signable_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(
            32 + 1
                + self.mesh_id.len()
                + 1
                + self.node_id.len()
                + 1
                + 2
                + self.bootstrap_token.len()
                + 8,
        );
        buf.extend_from_slice(&self.subject_public_key);
        buf.push(self.mesh_id.len() as u8);
        buf.extend_from_slice(self.mesh_id.as_bytes());
        buf.push(self.node_id.len() as u8);
        buf.extend_from_slice(self.node_id.as_bytes());
        buf.push(self.requested_tier.to_byte());
        buf.extend_from_slice(&(self.bootstrap_token.len() as u16).to_le_bytes());
        buf.extend_from_slice(&self.bootstrap_token);
        buf.extend_from_slice(&self.timestamp_ms.to_le_bytes());
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
        // Minimum: 32 + 1 + 0 + 1 + 0 + 1 + 2 + 0 + 8 + 64 = 109
        if data.len() < 109 {
            return Err(SecurityError::SerializationError(format!(
                "enrollment request too short: {} bytes (min 109)",
                data.len()
            )));
        }

        let mut pos = 0;

        let mut subject_public_key = [0u8; 32];
        subject_public_key.copy_from_slice(&data[pos..pos + 32]);
        pos += 32;

        let mesh_id_len = data[pos] as usize;
        pos += 1;

        if pos + mesh_id_len + 1 > data.len() {
            return Err(SecurityError::SerializationError(
                "enrollment request truncated at mesh_id".to_string(),
            ));
        }

        let mesh_id = String::from_utf8(data[pos..pos + mesh_id_len].to_vec())
            .map_err(|e| SecurityError::SerializationError(format!("invalid mesh_id: {e}")))?;
        pos += mesh_id_len;

        let node_id_len = data[pos] as usize;
        pos += 1;

        if pos + node_id_len + 1 + 2 > data.len() {
            return Err(SecurityError::SerializationError(
                "enrollment request truncated at node_id".to_string(),
            ));
        }

        let node_id = String::from_utf8(data[pos..pos + node_id_len].to_vec())
            .map_err(|e| SecurityError::SerializationError(format!("invalid node_id: {e}")))?;
        pos += node_id_len;

        let requested_tier = MeshTier::from_byte(data[pos])
            .ok_or_else(|| SecurityError::SerializationError("invalid tier byte".to_string()))?;
        pos += 1;

        let token_len = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;

        if pos + token_len + 8 + 64 > data.len() {
            return Err(SecurityError::SerializationError(
                "enrollment request truncated at token".to_string(),
            ));
        }

        let bootstrap_token = data[pos..pos + token_len].to_vec();
        pos += token_len;

        let timestamp_ms = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        pos += 8;

        let mut signature = [0u8; 64];
        signature.copy_from_slice(&data[pos..pos + 64]);

        Ok(Self {
            subject_public_key,
            mesh_id,
            node_id,
            requested_tier,
            bootstrap_token,
            timestamp_ms,
            signature,
        })
    }
}

/// Authority's response to an enrollment request.
///
/// On approval, includes the signed [`MeshCertificate`] and optionally the
/// formation secret needed for Iroh identity derivation.
#[derive(Clone, Debug)]
pub struct EnrollmentResponse {
    /// Enrollment decision.
    pub status: EnrollmentStatus,
    /// Signed certificate (present when status is Approved).
    pub certificate: Option<MeshCertificate>,
    /// Formation secret for HKDF-based Iroh identity derivation.
    /// Only provided on initial enrollment (not on re-enrollment).
    pub formation_secret: Option<Vec<u8>>,
    /// Response timestamp (Unix epoch milliseconds).
    pub timestamp_ms: u64,
}

impl EnrollmentResponse {
    /// Create an approval response with a signed certificate.
    pub fn approved(
        certificate: MeshCertificate,
        formation_secret: Option<Vec<u8>>,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            status: EnrollmentStatus::Approved,
            certificate: Some(certificate),
            formation_secret,
            timestamp_ms,
        }
    }

    /// Create a denial response.
    pub fn denied(reason: String, timestamp_ms: u64) -> Self {
        Self {
            status: EnrollmentStatus::Denied { reason },
            certificate: None,
            formation_secret: None,
            timestamp_ms,
        }
    }

    /// Create a pending response (request acknowledged, awaiting decision).
    pub fn pending(timestamp_ms: u64) -> Self {
        Self {
            status: EnrollmentStatus::Pending,
            certificate: None,
            formation_secret: None,
            timestamp_ms,
        }
    }
}

/// Trait for enrollment service backends.
///
/// Implementations handle the enrollment workflow — validating bootstrap tokens,
/// issuing certificates, and tracking enrollment status.
///
/// # Example implementations
///
/// - **Static**: Pre-provisioned list of allowed public keys + tokens
/// - **UDS Registry bridge**: Validates tokens against UDS Registry OIDC/PAT
/// - **Authority node**: Interactive approval via ATAK/WearTAK UX
#[async_trait::async_trait]
pub trait EnrollmentService: Send + Sync {
    /// Process an enrollment request.
    ///
    /// The implementation should:
    /// 1. Verify the request signature
    /// 2. Validate the bootstrap token
    /// 3. Issue a certificate on approval (or return Pending/Denied)
    async fn process_request(
        &self,
        request: &EnrollmentRequest,
    ) -> Result<EnrollmentResponse, SecurityError>;

    /// Check the enrollment status for a public key.
    async fn check_status(&self, subject_key: &[u8; 32])
        -> Result<EnrollmentStatus, SecurityError>;

    /// Revoke an enrollment (invalidate the certificate).
    async fn revoke(&self, subject_key: &[u8; 32], reason: String) -> Result<(), SecurityError>;
}

/// A static enrollment service that approves pre-provisioned nodes.
///
/// Useful for deployments where all nodes are known ahead of time.
/// Bootstrap tokens are checked against a pre-configured list.
pub struct StaticEnrollmentService {
    /// Authority keypair for signing certificates.
    authority: DeviceKeypair,
    /// Mesh ID for issued certificates.
    mesh_id: String,
    /// Valid bootstrap tokens mapped to (tier, permissions).
    allowed_tokens: std::collections::HashMap<Vec<u8>, (MeshTier, u8)>,
    /// Validity duration for issued certificates (milliseconds).
    validity_ms: u64,
}

impl StaticEnrollmentService {
    /// Create a new static enrollment service.
    pub fn new(authority: DeviceKeypair, mesh_id: String, validity_ms: u64) -> Self {
        Self {
            authority,
            mesh_id,
            allowed_tokens: std::collections::HashMap::new(),
            validity_ms,
        }
    }

    /// Register a valid bootstrap token with associated tier and permissions.
    pub fn add_token(&mut self, token: Vec<u8>, tier: MeshTier, permissions: u8) {
        self.allowed_tokens.insert(token, (tier, permissions));
    }
}

#[async_trait::async_trait]
impl EnrollmentService for StaticEnrollmentService {
    async fn process_request(
        &self,
        request: &EnrollmentRequest,
    ) -> Result<EnrollmentResponse, SecurityError> {
        // Verify request signature
        request.verify_signature()?;

        // Validate mesh ID
        if request.mesh_id != self.mesh_id {
            return Ok(EnrollmentResponse::denied(
                format!(
                    "mesh ID mismatch: expected {}, got {}",
                    self.mesh_id, request.mesh_id
                ),
                request.timestamp_ms,
            ));
        }

        // Look up bootstrap token
        let (tier, permissions) = match self.allowed_tokens.get(&request.bootstrap_token) {
            Some(entry) => *entry,
            None => {
                return Ok(EnrollmentResponse::denied(
                    "invalid bootstrap token".to_string(),
                    request.timestamp_ms,
                ));
            }
        };

        // Issue certificate
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let cert = MeshCertificate::new(
            request.subject_public_key,
            self.mesh_id.clone(),
            request.node_id.clone(),
            tier,
            permissions,
            now,
            now + self.validity_ms,
            self.authority.public_key_bytes(),
        )
        .signed(&self.authority);

        Ok(EnrollmentResponse::approved(cert, None, now))
    }

    async fn check_status(
        &self,
        _subject_key: &[u8; 32],
    ) -> Result<EnrollmentStatus, SecurityError> {
        // Static service doesn't track state — always pending until request
        Ok(EnrollmentStatus::Pending)
    }

    async fn revoke(&self, _subject_key: &[u8; 32], _reason: String) -> Result<(), SecurityError> {
        // Static service doesn't track revocations
        Err(SecurityError::Internal(
            "static enrollment service does not support revocation".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::certificate::permissions;
    use super::*;

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn test_enrollment_request_sign_verify() {
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let req = EnrollmentRequest::new(
            &member,
            "A1B2C3D4".to_string(),
            "tac-west-1".to_string(),
            MeshTier::Tactical,
            b"bootstrap-token-123".to_vec(),
            now,
        );

        assert!(req.verify_signature().is_ok());
        assert_eq!(req.subject_public_key, member.public_key_bytes());
        assert_eq!(req.mesh_id, "A1B2C3D4");
        assert_eq!(req.node_id, "tac-west-1");
    }

    #[test]
    fn test_enrollment_request_encode_decode() {
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let req = EnrollmentRequest::new(
            &member,
            "A1B2C3D4".to_string(),
            "edge-unit-7".to_string(),
            MeshTier::Edge,
            b"token".to_vec(),
            now,
        );

        let encoded = req.encode();
        let decoded = EnrollmentRequest::decode(&encoded).unwrap();

        assert_eq!(decoded.subject_public_key, req.subject_public_key);
        assert_eq!(decoded.mesh_id, req.mesh_id);
        assert_eq!(decoded.node_id, "edge-unit-7");
        assert_eq!(decoded.requested_tier, req.requested_tier);
        assert_eq!(decoded.bootstrap_token, req.bootstrap_token);
        assert_eq!(decoded.timestamp_ms, req.timestamp_ms);
        assert!(decoded.verify_signature().is_ok());
    }

    #[test]
    fn test_enrollment_request_decode_too_short() {
        assert!(EnrollmentRequest::decode(&[0u8; 10]).is_err());
    }

    #[test]
    fn test_enrollment_response_approved() {
        let authority = DeviceKeypair::generate();
        let now = now_ms();

        let cert = MeshCertificate::new_root(
            &authority,
            "DEADBEEF".to_string(),
            "enterprise-0".to_string(),
            MeshTier::Enterprise,
            now,
            now + 3600000,
        );

        let resp = EnrollmentResponse::approved(cert, Some(b"secret".to_vec()), now);
        assert_eq!(resp.status, EnrollmentStatus::Approved);
        assert!(resp.certificate.is_some());
        assert!(resp.formation_secret.is_some());
    }

    #[test]
    fn test_enrollment_response_denied() {
        let now = now_ms();
        let resp = EnrollmentResponse::denied("bad token".to_string(), now);
        assert_eq!(
            resp.status,
            EnrollmentStatus::Denied {
                reason: "bad token".to_string()
            }
        );
        assert!(resp.certificate.is_none());
    }

    #[tokio::test]
    async fn test_static_enrollment_service_approve() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();
        let validity = 24 * 60 * 60 * 1000; // 24 hours

        let mut service =
            StaticEnrollmentService::new(authority.clone(), "DEADBEEF".to_string(), validity);
        service.add_token(
            b"valid-token".to_vec(),
            MeshTier::Tactical,
            permissions::STANDARD,
        );

        let req = EnrollmentRequest::new(
            &member,
            "DEADBEEF".to_string(),
            "tac-node-1".to_string(),
            MeshTier::Tactical,
            b"valid-token".to_vec(),
            now,
        );

        let resp = service.process_request(&req).await.unwrap();
        assert_eq!(resp.status, EnrollmentStatus::Approved);

        let cert = resp.certificate.unwrap();
        assert!(cert.verify().is_ok());
        assert_eq!(cert.subject_public_key, member.public_key_bytes());
        assert_eq!(cert.node_id, "tac-node-1");
        assert_eq!(cert.tier, MeshTier::Tactical);
        assert_eq!(cert.permissions, permissions::STANDARD);
        assert_eq!(cert.issuer_public_key, authority.public_key_bytes());
    }

    #[tokio::test]
    async fn test_static_enrollment_service_deny_bad_token() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let service = StaticEnrollmentService::new(authority, "DEADBEEF".to_string(), 3600000);

        let req = EnrollmentRequest::new(
            &member,
            "DEADBEEF".to_string(),
            "tac-node-2".to_string(),
            MeshTier::Tactical,
            b"invalid-token".to_vec(),
            now,
        );

        let resp = service.process_request(&req).await.unwrap();
        match resp.status {
            EnrollmentStatus::Denied { reason } => {
                assert!(reason.contains("invalid bootstrap token"));
            }
            other => panic!("expected Denied, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_static_enrollment_service_deny_wrong_mesh() {
        let authority = DeviceKeypair::generate();
        let member = DeviceKeypair::generate();
        let now = now_ms();

        let mut service = StaticEnrollmentService::new(authority, "DEADBEEF".to_string(), 3600000);
        service.add_token(b"token".to_vec(), MeshTier::Tactical, permissions::STANDARD);

        let req = EnrollmentRequest::new(
            &member,
            "WRONG_MESH".to_string(),
            "tac-node-3".to_string(),
            MeshTier::Tactical,
            b"token".to_vec(),
            now,
        );

        let resp = service.process_request(&req).await.unwrap();
        match resp.status {
            EnrollmentStatus::Denied { reason } => {
                assert!(reason.contains("mesh ID mismatch"));
            }
            other => panic!("expected Denied, got {:?}", other),
        }
    }

    #[test]
    fn test_enrollment_status_byte() {
        assert_eq!(EnrollmentStatus::Pending.to_byte(), 0);
        assert_eq!(EnrollmentStatus::Approved.to_byte(), 1);
        assert_eq!(
            EnrollmentStatus::Denied {
                reason: "x".to_string()
            }
            .to_byte(),
            2
        );
        assert_eq!(
            EnrollmentStatus::Revoked {
                reason: "x".to_string()
            }
            .to_byte(),
            3
        );
    }
}
