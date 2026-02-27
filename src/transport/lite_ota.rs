//! OTA firmware sender for Peat-Lite nodes.
//!
//! Manages firmware delivery from a Full Eche node to one or more Lite peers
//! using the OTA wire protocol (message types 0x10-0x16) over the existing
//! `LiteMeshTransport` UDP transport.
//!
//! # Protocol
//!
//! Stop-and-wait: send one chunk, wait for ACK, retransmit after timeout.
//! This is simple, correct, and requires no buffering on the ESP32 side.

use super::lite::{LiteMeshTransport, LiteMessage};
use super::NodeId;
use crate::security::DeviceKeypair;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

// Re-export OTA constants from peat-lite-protocol (single source of truth)
use peat_lite_protocol::ota::{OTA_ABORT_TOO_MANY_RETRIES, OTA_RESULT_SUCCESS};
pub use peat_lite_protocol::ota::{OTA_CHUNK_DATA_SIZE, OTA_FLAG_SIGNED};

/// Timeout before retransmitting a chunk
const CHUNK_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum retries per chunk before aborting
const MAX_RETRIES_PER_CHUNK: u32 = 5;

/// Overall session timeout
const SESSION_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

/// A firmware image ready for OTA delivery
#[derive(Clone)]
pub struct FirmwareImage {
    /// Raw firmware bytes
    pub data: Vec<u8>,
    /// SHA256 digest of the firmware
    pub sha256: [u8; 32],
    /// Version string (max 16 bytes, null-padded)
    pub version: [u8; 16],
    /// Number of chunks needed
    pub total_chunks: u16,
    /// Size of each chunk (except possibly the last)
    pub chunk_size: u16,
    /// Ed25519 signature over the SHA256 digest (set by `sign()`)
    pub signature: Option<[u8; 64]>,
}

impl FirmwareImage {
    /// Create a FirmwareImage from raw bytes and a version string
    pub fn new(data: Vec<u8>, version: &str) -> Self {
        let sha256 = {
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let result = hasher.finalize();
            let mut digest = [0u8; 32];
            digest.copy_from_slice(&result);
            digest
        };

        let chunk_size = OTA_CHUNK_DATA_SIZE as u16;
        let total_chunks = data.len().div_ceil(OTA_CHUNK_DATA_SIZE) as u16;

        let mut ver_buf = [0u8; 16];
        let ver_bytes = version.as_bytes();
        let copy_len = ver_bytes.len().min(15); // Leave room for null terminator
        ver_buf[..copy_len].copy_from_slice(&ver_bytes[..copy_len]);

        Self {
            data,
            sha256,
            version: ver_buf,
            total_chunks,
            chunk_size,
            signature: None,
        }
    }

    /// Sign the SHA256 digest with the given keypair.
    /// Sets `self.signature` to the 64-byte Ed25519 signature.
    pub fn sign(&mut self, keypair: &DeviceKeypair) {
        let sig = keypair.sign(&self.sha256);
        self.signature = Some(sig.to_bytes());
    }

    /// Get chunk data for the given chunk number
    pub fn chunk(&self, chunk_num: u16) -> Option<&[u8]> {
        let start = chunk_num as usize * self.chunk_size as usize;
        if start >= self.data.len() {
            return None;
        }
        let end = (start + self.chunk_size as usize).min(self.data.len());
        Some(&self.data[start..end])
    }
}

/// State of an OTA session with a single peer
#[derive(Debug, Clone)]
pub enum OtaSenderState {
    /// Offer sent, waiting for accept
    Offered { sent_at: Instant, retries: u32 },
    /// Transferring chunks
    Transferring {
        current_chunk: u16,
        last_sent_at: Instant,
        retries: u32,
    },
    /// All chunks sent, sent OtaComplete, waiting for OtaResult
    WaitingResult { sent_at: Instant, retries: u32 },
    /// OTA completed successfully
    Complete,
    /// OTA failed
    Failed { reason: String },
}

/// Per-peer OTA session info
pub struct OtaSession {
    pub session_id: u16,
    pub peer_id: NodeId,
    pub firmware: FirmwareImage,
    pub state: OtaSenderState,
    pub started_at: Instant,
    pub chunks_acked: u16,
}

impl OtaSession {
    /// Get progress as percentage
    pub fn progress_percent(&self) -> u8 {
        if self.firmware.total_chunks == 0 {
            return 0;
        }
        ((self.chunks_acked as u32 * 100) / self.firmware.total_chunks as u32) as u8
    }

    /// Check if session has timed out overall
    pub fn is_timed_out(&self) -> bool {
        self.started_at.elapsed() > SESSION_TIMEOUT
    }
}

/// OTA sender managing sessions for multiple peers
pub struct OtaSender {
    transport: Arc<LiteMeshTransport>,
    sessions: Arc<RwLock<HashMap<String, OtaSession>>>,
    next_session_id: Arc<RwLock<u16>>,
    /// Optional signing keypair for OTA offers
    signing_keypair: Option<Arc<DeviceKeypair>>,
}

impl OtaSender {
    /// Create a new OTA sender, optionally with a signing keypair
    pub fn new(
        transport: Arc<LiteMeshTransport>,
        signing_keypair: Option<Arc<DeviceKeypair>>,
    ) -> Self {
        Self {
            transport,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            next_session_id: Arc::new(RwLock::new(1)),
            signing_keypair,
        }
    }

    /// Offer firmware to a specific peer. Returns the session ID.
    pub async fn offer_to_peer(
        &self,
        peer_id: &NodeId,
        mut firmware: FirmwareImage,
    ) -> Result<u16, String> {
        let session_id = {
            let mut id = self.next_session_id.write().await;
            let sid = *id;
            *id = id.wrapping_add(1);
            if *id == 0 {
                *id = 1;
            }
            sid
        };

        // Sign the firmware digest if we have a keypair
        let mut flags: u16 = 0;
        if let Some(keypair) = &self.signing_keypair {
            firmware.sign(keypair);
            flags |= OTA_FLAG_SIGNED;
        }

        // Build and send OtaOffer
        let msg = LiteMessage::ota_offer(
            self.transport.local_node_id,
            &firmware.version,
            firmware.data.len() as u32,
            firmware.total_chunks,
            firmware.chunk_size,
            &firmware.sha256,
            session_id,
            flags,
            firmware.signature.as_ref(),
        );

        self.transport
            .send_to(peer_id, &msg)
            .await
            .map_err(|e| format!("Failed to send OTA offer: {}", e))?;

        let session = OtaSession {
            session_id,
            peer_id: peer_id.clone(),
            firmware,
            state: OtaSenderState::Offered {
                sent_at: Instant::now(),
                retries: 0,
            },
            started_at: Instant::now(),
            chunks_acked: 0,
        };

        self.sessions
            .write()
            .await
            .insert(peer_id.as_str().to_string(), session);

        info!(
            peer = %peer_id,
            session_id = session_id,
            "OTA offer sent"
        );

        Ok(session_id)
    }

    /// Handle an OtaAccept message from a peer
    pub async fn handle_accept(&self, peer_id: &str, payload: &[u8]) {
        if payload.len() < 4 {
            return;
        }

        let session_id = u16::from_le_bytes(payload[0..2].try_into().unwrap());
        let resume_chunk = u16::from_le_bytes(payload[2..4].try_into().unwrap());

        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(peer_id) {
            if session.session_id != session_id {
                warn!(peer = %peer_id, "OtaAccept session mismatch");
                return;
            }

            info!(
                peer = %peer_id,
                session_id = session_id,
                resume_chunk = resume_chunk,
                "OTA accepted, starting transfer"
            );

            session.chunks_acked = resume_chunk;
            session.state = OtaSenderState::Transferring {
                current_chunk: resume_chunk,
                last_sent_at: Instant::now(),
                retries: 0,
            };

            // Send first chunk
            drop(sessions);
            self.send_chunk(peer_id, resume_chunk).await;
        }
    }

    /// Handle an OtaAck message from a peer
    pub async fn handle_ack(&self, peer_id: &str, payload: &[u8]) {
        if payload.len() < 4 {
            return;
        }

        let session_id = u16::from_le_bytes(payload[0..2].try_into().unwrap());
        let acked_chunk = u16::from_le_bytes(payload[2..4].try_into().unwrap());

        let next_chunk = {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(peer_id) {
                if session.session_id != session_id {
                    return;
                }

                session.chunks_acked = acked_chunk + 1;

                let next = acked_chunk + 1;
                if next >= session.firmware.total_chunks {
                    // All chunks sent and ACK'd -> send OtaComplete
                    session.state = OtaSenderState::WaitingResult {
                        sent_at: Instant::now(),
                        retries: 0,
                    };
                    debug!(peer = %peer_id, "All chunks ACK'd, sending OtaComplete");
                    None
                } else {
                    session.state = OtaSenderState::Transferring {
                        current_chunk: next,
                        last_sent_at: Instant::now(),
                        retries: 0,
                    };
                    Some(next)
                }
            } else {
                return;
            }
        };

        if let Some(chunk_num) = next_chunk {
            self.send_chunk(peer_id, chunk_num).await;
        } else {
            self.send_complete(peer_id).await;
        }
    }

    /// Handle an OtaResult message from a peer
    pub async fn handle_result(&self, peer_id: &str, payload: &[u8]) {
        if payload.len() < 3 {
            return;
        }

        let session_id = u16::from_le_bytes(payload[0..2].try_into().unwrap());
        let result_code = payload[2];

        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(peer_id) {
            if session.session_id != session_id {
                return;
            }

            if result_code == OTA_RESULT_SUCCESS {
                info!(peer = %peer_id, "OTA update successful! Peer will reboot.");
                session.state = OtaSenderState::Complete;
            } else {
                let reason = match result_code {
                    0x01 => "hash mismatch",
                    0x02 => "flash error",
                    0x03 => "invalid offer",
                    0x04 => "signature invalid",
                    0x05 => "signature required",
                    _ => "unknown",
                };
                error!(peer = %peer_id, result_code, reason, "OTA update failed");
                session.state = OtaSenderState::Failed {
                    reason: reason.to_string(),
                };
            }
        }
    }

    /// Handle an OtaAbort message from a peer
    pub async fn handle_abort(&self, peer_id: &str, payload: &[u8]) {
        if payload.len() < 3 {
            return;
        }

        let session_id = u16::from_le_bytes(payload[0..2].try_into().unwrap());
        let reason = payload[2];

        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(peer_id) {
            if session.session_id == session_id {
                warn!(peer = %peer_id, reason, "OTA aborted by peer");
                session.state = OtaSenderState::Failed {
                    reason: format!("peer aborted: reason={}", reason),
                };
            }
        }
    }

    /// Periodic tick: check for timeouts and retransmit
    pub async fn tick(&self) {
        let peers_to_process: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions.keys().cloned().collect()
        };

        for peer_id in peers_to_process {
            let action = {
                let mut sessions = self.sessions.write().await;
                if let Some(session) = sessions.get_mut(&peer_id) {
                    // Check overall timeout
                    if session.is_timed_out() {
                        session.state = OtaSenderState::Failed {
                            reason: "session timeout".to_string(),
                        };
                        Some(TickAction::Abort(session.session_id))
                    } else {
                        match &session.state {
                            OtaSenderState::Offered { sent_at, retries } => {
                                if sent_at.elapsed() > CHUNK_TIMEOUT {
                                    if *retries >= MAX_RETRIES_PER_CHUNK {
                                        session.state = OtaSenderState::Failed {
                                            reason: "offer not accepted".to_string(),
                                        };
                                        Some(TickAction::Abort(session.session_id))
                                    } else {
                                        Some(TickAction::RetransmitOffer(
                                            session.session_id,
                                            *retries + 1,
                                        ))
                                    }
                                } else {
                                    None
                                }
                            }
                            OtaSenderState::Transferring {
                                current_chunk,
                                last_sent_at,
                                retries,
                            } => {
                                if last_sent_at.elapsed() > CHUNK_TIMEOUT {
                                    if *retries >= MAX_RETRIES_PER_CHUNK {
                                        session.state = OtaSenderState::Failed {
                                            reason: format!("chunk {} retry limit", current_chunk),
                                        };
                                        Some(TickAction::Abort(session.session_id))
                                    } else {
                                        let chunk = *current_chunk;
                                        let new_retries = *retries + 1;
                                        session.state = OtaSenderState::Transferring {
                                            current_chunk: chunk,
                                            last_sent_at: Instant::now(),
                                            retries: new_retries,
                                        };
                                        Some(TickAction::RetransmitChunk(chunk))
                                    }
                                } else {
                                    None
                                }
                            }
                            OtaSenderState::WaitingResult { sent_at, retries } => {
                                if sent_at.elapsed() > CHUNK_TIMEOUT {
                                    if *retries >= MAX_RETRIES_PER_CHUNK {
                                        session.state = OtaSenderState::Failed {
                                            reason: "no result received".to_string(),
                                        };
                                        Some(TickAction::Abort(session.session_id))
                                    } else {
                                        let new_retries = *retries + 1;
                                        session.state = OtaSenderState::WaitingResult {
                                            sent_at: Instant::now(),
                                            retries: new_retries,
                                        };
                                        Some(TickAction::RetransmitComplete)
                                    }
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    }
                } else {
                    None
                }
            };

            match action {
                Some(TickAction::RetransmitOffer(session_id, new_retries)) => {
                    debug!(peer = %peer_id, retries = new_retries, "Retransmitting OTA offer");
                    // Re-send offer
                    let sessions = self.sessions.read().await;
                    if let Some(session) = sessions.get(&peer_id) {
                        let flags = if session.firmware.signature.is_some() {
                            OTA_FLAG_SIGNED
                        } else {
                            0
                        };
                        let msg = LiteMessage::ota_offer(
                            self.transport.local_node_id,
                            &session.firmware.version,
                            session.firmware.data.len() as u32,
                            session.firmware.total_chunks,
                            session.firmware.chunk_size,
                            &session.firmware.sha256,
                            session_id,
                            flags,
                            session.firmware.signature.as_ref(),
                        );
                        let _ = self.transport.send_to(&session.peer_id, &msg).await;
                    }
                    drop(sessions);
                    let mut sessions = self.sessions.write().await;
                    if let Some(session) = sessions.get_mut(&peer_id) {
                        session.state = OtaSenderState::Offered {
                            sent_at: Instant::now(),
                            retries: new_retries,
                        };
                    }
                }
                Some(TickAction::RetransmitChunk(chunk_num)) => {
                    debug!(peer = %peer_id, chunk = chunk_num, "Retransmitting chunk");
                    self.send_chunk(&peer_id, chunk_num).await;
                }
                Some(TickAction::RetransmitComplete) => {
                    debug!(peer = %peer_id, "Retransmitting OtaComplete");
                    self.send_complete(&peer_id).await;
                }
                Some(TickAction::Abort(session_id)) => {
                    warn!(peer = %peer_id, "Aborting OTA session");
                    let msg = LiteMessage::ota_abort(
                        self.transport.local_node_id,
                        session_id,
                        OTA_ABORT_TOO_MANY_RETRIES,
                    );
                    let peer_node_id = NodeId::new(peer_id.clone());
                    let _ = self.transport.send_to(&peer_node_id, &msg).await;
                }
                None => {}
            }
        }
    }

    /// Get session status for a peer
    pub async fn get_status(&self, peer_id: &str) -> Option<OtaStatusInfo> {
        let sessions = self.sessions.read().await;
        sessions.get(peer_id).map(|session| OtaStatusInfo {
            session_id: session.session_id,
            state: match &session.state {
                OtaSenderState::Offered { .. } => "offered".to_string(),
                OtaSenderState::Transferring { .. } => "transferring".to_string(),
                OtaSenderState::WaitingResult { .. } => "waiting_result".to_string(),
                OtaSenderState::Complete => "complete".to_string(),
                OtaSenderState::Failed { reason } => format!("failed: {}", reason),
            },
            progress_percent: session.progress_percent(),
            chunks_sent: session.chunks_acked,
            total_chunks: session.firmware.total_chunks,
            elapsed_secs: session.started_at.elapsed().as_secs(),
        })
    }

    /// Send a data chunk to a peer
    async fn send_chunk(&self, peer_id: &str, chunk_num: u16) {
        let (session_id, chunk_data) = {
            let sessions = self.sessions.read().await;
            if let Some(session) = sessions.get(peer_id) {
                if let Some(data) = session.firmware.chunk(chunk_num) {
                    (session.session_id, data.to_vec())
                } else {
                    return;
                }
            } else {
                return;
            }
        };

        let msg = LiteMessage::ota_data(
            self.transport.local_node_id,
            session_id,
            chunk_num,
            &chunk_data,
        );

        let peer_node_id = NodeId::new(peer_id.to_string());
        if let Err(e) = self.transport.send_to(&peer_node_id, &msg).await {
            warn!(peer = %peer_id, chunk = chunk_num, error = %e, "Failed to send OTA chunk");
        }
    }

    /// Send OtaComplete to a peer
    async fn send_complete(&self, peer_id: &str) {
        let session_id = {
            let sessions = self.sessions.read().await;
            sessions.get(peer_id).map(|s| s.session_id)
        };

        if let Some(session_id) = session_id {
            let msg = LiteMessage::ota_complete(self.transport.local_node_id, session_id);
            let peer_node_id = NodeId::new(peer_id.to_string());
            let _ = self.transport.send_to(&peer_node_id, &msg).await;
        }
    }
}

/// Internal action from tick processing
enum TickAction {
    RetransmitOffer(u16, u32),
    RetransmitChunk(u16),
    RetransmitComplete,
    Abort(u16),
}

/// OTA status information for API responses
#[derive(Debug, Clone, serde::Serialize)]
pub struct OtaStatusInfo {
    pub session_id: u16,
    pub state: String,
    pub progress_percent: u8,
    pub chunks_sent: u16,
    pub total_chunks: u16,
    pub elapsed_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_firmware_image_new() {
        let data = vec![0xAA; 1000];
        let fw = FirmwareImage::new(data.clone(), "1.0.0");

        assert_eq!(fw.data.len(), 1000);
        assert_eq!(fw.chunk_size, OTA_CHUNK_DATA_SIZE as u16);
        assert_eq!(fw.total_chunks, 3); // ceil(1000 / 448) = 3

        // Verify version
        assert_eq!(&fw.version[..5], b"1.0.0");
        assert_eq!(fw.version[5], 0);

        // Verify SHA256
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(fw.sha256, expected);
    }

    #[test]
    fn test_firmware_image_chunks() {
        let data = vec![0xBB; 1000];
        let fw = FirmwareImage::new(data, "0.1.0");

        // Chunk 0: 448 bytes
        let c0 = fw.chunk(0).unwrap();
        assert_eq!(c0.len(), 448);

        // Chunk 1: 448 bytes
        let c1 = fw.chunk(1).unwrap();
        assert_eq!(c1.len(), 448);

        // Chunk 2: remainder (1000 - 896 = 104 bytes)
        let c2 = fw.chunk(2).unwrap();
        assert_eq!(c2.len(), 104);

        // Chunk 3: out of bounds
        assert!(fw.chunk(3).is_none());
    }

    #[test]
    fn test_firmware_image_exact_multiple() {
        let data = vec![0xCC; 896]; // Exactly 2 chunks
        let fw = FirmwareImage::new(data, "2.0");

        assert_eq!(fw.total_chunks, 2);
        assert_eq!(fw.chunk(0).unwrap().len(), 448);
        assert_eq!(fw.chunk(1).unwrap().len(), 448);
        assert!(fw.chunk(2).is_none());
    }

    #[test]
    fn test_firmware_image_single_chunk() {
        let data = vec![0xDD; 100];
        let fw = FirmwareImage::new(data, "0.0.1");

        assert_eq!(fw.total_chunks, 1);
        assert_eq!(fw.chunk(0).unwrap().len(), 100);
        assert!(fw.chunk(1).is_none());
    }

    #[test]
    fn test_ota_status_info_serialize() {
        let info = OtaStatusInfo {
            session_id: 42,
            state: "transferring".to_string(),
            progress_percent: 50,
            chunks_sent: 5,
            total_chunks: 10,
            elapsed_secs: 30,
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"session_id\":42"));
        assert!(json.contains("\"progress_percent\":50"));
    }

    #[test]
    fn test_version_truncation() {
        let data = vec![0; 10];
        let fw = FirmwareImage::new(data, "this-is-a-very-long-version-string");

        // Should be truncated to 15 bytes + null
        let ver_str = std::str::from_utf8(
            &fw.version[..fw.version.iter().position(|&b| b == 0).unwrap_or(16)],
        )
        .unwrap();
        assert_eq!(ver_str, "this-is-a-very-");
    }
}
