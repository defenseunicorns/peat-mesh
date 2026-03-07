//! Peat-Lite transport for embedded/constrained devices
//!
//! This transport enables Full Peat nodes to communicate with Peat-Lite nodes
//! (ESP32, M5Stack, etc.) over simple UDP. It implements the ADR-035 wire protocol.
//!
//! # Architecture
//!
//! - Listens on UDP port for Peat-Lite messages (default 5555)
//! - Maintains virtual connections to Lite nodes based on heartbeats
//! - Translates primitive CRDTs to/from Automerge documents
//! - Emits PeerEvents for connection lifecycle
//!
//! # Wire Protocol (ADR-035)
//!
//! ```text
//! ┌──────────┬─────────┬──────────┬──────────┬──────────┬──────────────┐
//! │  MAGIC   │ Version │   Type   │  Flags   │  NodeID  │   SeqNum     │
//! │  4 bytes │ 1 byte  │  1 byte  │  2 bytes │  4 bytes │   4 bytes    │
//! ├──────────┴─────────┴──────────┴──────────┴──────────┴──────────────┤
//! │                          Payload                                    │
//! │                       (variable, max 496 bytes)                     │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use peat_mesh::transport::lite::{LiteMeshTransport, LiteTransportConfig};
//!
//! let config = LiteTransportConfig {
//!     listen_port: 5555,
//!     broadcast_port: 5555,
//!     peer_timeout_secs: 30,
//! };
//!
//! let transport = LiteMeshTransport::new(config);
//! transport.start().await?;
//!
//! // Subscribe to peer events
//! let mut events = transport.subscribe_peer_events();
//! while let Some(event) = events.recv().await {
//!     match event {
//!         PeerEvent::Connected { peer_id, .. } => {
//!             println!("Lite node connected: {}", peer_id);
//!         }
//!         _ => {}
//!     }
//! }
//! ```

use async_trait::async_trait;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};

// For sync-context locks (MeshTransport trait methods are sync)
use std::sync::Mutex as StdMutex;

use super::{
    ConnectionHealth, ConnectionState, DisconnectReason, MeshConnection, MeshTransport, NodeId,
    PeerEvent, PeerEventReceiver, PeerEventSender, Result, TransportError,
    PEER_EVENT_CHANNEL_CAPACITY,
};

// Re-export wire protocol types from peat-lite (single source of truth)
pub use peat_lite::{
    CrdtType, MessageType, DEFAULT_PORT, HEADER_SIZE, MAGIC, MAX_PACKET_SIZE, PROTOCOL_VERSION,
};

/// Type alias for CRDT callback to avoid clippy::type_complexity
type CrdtCallback = Arc<StdMutex<Option<Box<dyn Fn(&str, &str, CrdtType, &[u8]) + Send + Sync>>>>;

/// Type alias for query callback
type QueryCallback =
    Arc<StdMutex<Option<Box<dyn Fn(&QueryRequest) -> Option<Vec<u8>> + Send + Sync>>>>;

/// Type alias for OTA message callback
/// (peer_id, message_type, payload)
type OtaCallback = Arc<StdMutex<Option<Box<dyn Fn(&str, MessageType, &[u8]) + Send + Sync>>>>;

/// Element in an Observed-Remove Set (OrSet)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrSetElement {
    /// Node that added this element
    pub tag_node_id: u32,
    /// Sequence number of the add operation
    pub tag_seq: u32,
    /// Element value (arbitrary bytes)
    pub value: Vec<u8>,
}

/// Query request message for requesting specific CRDT state
#[derive(Debug, Clone)]
pub struct QueryRequest {
    /// CRDT type being queried
    pub crdt_type: CrdtType,
    /// Collection name
    pub collection: String,
    /// Optional document ID (None = all docs in collection)
    pub doc_id: Option<String>,
}

impl QueryRequest {
    /// Encode a query request to wire bytes
    pub fn encode(&self) -> Vec<u8> {
        let collection_bytes = self.collection.as_bytes();
        let doc_id_bytes = self.doc_id.as_deref().unwrap_or("").as_bytes();

        let mut buf = Vec::with_capacity(3 + collection_bytes.len() + doc_id_bytes.len());
        buf.push(self.crdt_type as u8);
        buf.push(collection_bytes.len() as u8);
        buf.extend_from_slice(collection_bytes);
        buf.push(doc_id_bytes.len() as u8);
        buf.extend_from_slice(doc_id_bytes);
        buf
    }

    /// Decode a query request from wire bytes
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < 3 {
            return None;
        }

        let crdt_type = CrdtType::from_u8(data[0])?;
        let collection_len = data[1] as usize;

        if data.len() < 2 + collection_len + 1 {
            return None;
        }

        let collection = std::str::from_utf8(&data[2..2 + collection_len])
            .ok()?
            .to_string();

        let doc_id_offset = 2 + collection_len;
        let doc_id_len = data[doc_id_offset] as usize;

        if data.len() < doc_id_offset + 1 + doc_id_len {
            return None;
        }

        let doc_id = if doc_id_len > 0 {
            Some(
                std::str::from_utf8(&data[doc_id_offset + 1..doc_id_offset + 1 + doc_id_len])
                    .ok()?
                    .to_string(),
            )
        } else {
            None
        };

        Some(Self {
            crdt_type,
            collection,
            doc_id,
        })
    }
}

/// Capability flags (ADR-035) — re-exported from peat-lite
pub use peat_lite::NodeCapabilities as LiteCapabilities;

/// Backward-compatibility alias: `FULL_CRDT` maps to `DOCUMENT_CRDT` in
/// peat-lite-protocol (same bit 0x0004).
pub const FULL_CRDT: u16 = LiteCapabilities::DOCUMENT_CRDT;

/// Extension helpers for LiteCapabilities that accept `&[u8]` (peat-mesh convenience).
pub trait LiteCapabilitiesExt {
    fn from_bytes(bytes: &[u8]) -> LiteCapabilities;
}

impl LiteCapabilitiesExt for LiteCapabilities {
    fn from_bytes(bytes: &[u8]) -> LiteCapabilities {
        if bytes.len() >= 2 {
            LiteCapabilities::from_bits(u16::from_le_bytes([bytes[0], bytes[1]]))
        } else {
            LiteCapabilities::empty()
        }
    }
}

// =============================================================================
// Parsed Message
// =============================================================================

/// Parsed Peat-Lite message
#[derive(Debug, Clone)]
pub struct LiteMessage {
    pub msg_type: MessageType,
    pub flags: u16,
    pub node_id: u32,
    pub seq_num: u32,
    pub payload: Vec<u8>,
}

impl LiteMessage {
    /// Decode a message from bytes
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 16 {
            return None;
        }

        // Check magic
        if buf[0..4] != MAGIC {
            return None;
        }

        // Check version
        if buf[4] != PROTOCOL_VERSION {
            return None;
        }

        let msg_type = MessageType::from_u8(buf[5])?;
        let flags = u16::from_le_bytes(buf[6..8].try_into().ok()?);
        let node_id = u32::from_le_bytes(buf[8..12].try_into().ok()?);
        let seq_num = u32::from_le_bytes(buf[12..16].try_into().ok()?);

        let payload = if buf.len() > 16 {
            buf[16..].to_vec()
        } else {
            Vec::new()
        };

        Some(Self {
            msg_type,
            flags,
            node_id,
            seq_num,
            payload,
        })
    }

    /// Encode a message to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 + self.payload.len());
        buf.extend_from_slice(&MAGIC);
        buf.push(PROTOCOL_VERSION);
        buf.push(self.msg_type as u8);
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.extend_from_slice(&self.node_id.to_le_bytes());
        buf.extend_from_slice(&self.seq_num.to_le_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Create an ACK message
    pub fn ack(node_id: u32, ack_seq: u32) -> Self {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&ack_seq.to_le_bytes());
        Self {
            msg_type: MessageType::Ack,
            flags: 0,
            node_id,
            seq_num: 0,
            payload,
        }
    }

    /// Create a QUERY message
    pub fn query(node_id: u32, seq_num: u32, request: &QueryRequest) -> Self {
        Self {
            msg_type: MessageType::Query,
            flags: 0,
            node_id,
            seq_num,
            payload: request.encode(),
        }
    }

    /// Create a DATA message with CRDT payload
    pub fn data(node_id: u32, seq_num: u32, crdt_type: CrdtType, crdt_data: &[u8]) -> Self {
        let mut payload = Vec::with_capacity(1 + crdt_data.len());
        payload.push(crdt_type as u8);
        payload.extend_from_slice(crdt_data);
        Self {
            msg_type: MessageType::Data,
            flags: 0,
            node_id,
            seq_num,
            payload,
        }
    }

    /// Create an OTA offer message
    ///
    /// Without signature (legacy, 76 bytes):
    ///   version(16) + size(4) + total_chunks(2) + chunk_size(2) + sha256(32)
    ///   + session_id(2) + flags(2) + reserved(16) = 76 bytes
    ///
    /// With signature (v2, 140 bytes):
    ///   version(16) + size(4) + total_chunks(2) + chunk_size(2) + sha256(32)
    ///   + session_id(2) + flags(2) + signature(64) + reserved(16) = 140 bytes
    #[allow(clippy::too_many_arguments)]
    pub fn ota_offer(
        node_id: u32,
        version: &[u8; 16],
        firmware_size: u32,
        total_chunks: u16,
        chunk_size: u16,
        sha256: &[u8; 32],
        session_id: u16,
        flags: u16,
        signature: Option<&[u8; 64]>,
    ) -> Self {
        let capacity = if signature.is_some() { 140 } else { 76 };
        let mut payload = Vec::with_capacity(capacity);
        payload.extend_from_slice(version);
        payload.extend_from_slice(&firmware_size.to_le_bytes());
        payload.extend_from_slice(&total_chunks.to_le_bytes());
        payload.extend_from_slice(&chunk_size.to_le_bytes());
        payload.extend_from_slice(sha256);
        payload.extend_from_slice(&session_id.to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        if let Some(sig) = signature {
            payload.extend_from_slice(sig);
            payload.extend_from_slice(&[0u8; 16]); // reserved (140 total)
        } else {
            payload.extend_from_slice(&[0u8; 16]); // reserved (76 total)
        }
        Self {
            msg_type: MessageType::OtaOffer,
            flags: 0,
            node_id,
            seq_num: 0,
            payload,
        }
    }

    /// Create an OTA data chunk message
    ///
    /// Payload: session_id(2) + chunk_num(2) + chunk_len(2) + data(<=448)
    pub fn ota_data(node_id: u32, session_id: u16, chunk_num: u16, data: &[u8]) -> Self {
        let mut payload = Vec::with_capacity(6 + data.len());
        payload.extend_from_slice(&session_id.to_le_bytes());
        payload.extend_from_slice(&chunk_num.to_le_bytes());
        payload.extend_from_slice(&(data.len() as u16).to_le_bytes());
        payload.extend_from_slice(data);
        Self {
            msg_type: MessageType::OtaData,
            flags: 0,
            node_id,
            seq_num: 0,
            payload,
        }
    }

    /// Create an OTA complete message
    pub fn ota_complete(node_id: u32, session_id: u16) -> Self {
        let mut payload = Vec::with_capacity(2);
        payload.extend_from_slice(&session_id.to_le_bytes());
        Self {
            msg_type: MessageType::OtaComplete,
            flags: 0,
            node_id,
            seq_num: 0,
            payload,
        }
    }

    /// Create an OTA abort message
    pub fn ota_abort(node_id: u32, session_id: u16, reason: u8) -> Self {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&session_id.to_le_bytes());
        payload.push(reason);
        payload.push(0); // reserved
        Self {
            msg_type: MessageType::OtaAbort,
            flags: 0,
            node_id,
            seq_num: 0,
            payload,
        }
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for Peat-Lite transport
#[derive(Debug, Clone)]
pub struct LiteTransportConfig {
    /// Port to listen on for incoming messages
    pub listen_port: u16,

    /// Port to broadcast to (usually same as listen_port)
    pub broadcast_port: u16,

    /// Seconds before considering a peer offline (no heartbeat)
    pub peer_timeout_secs: u64,

    /// Enable broadcast sending (for bidirectional sync)
    pub enable_broadcast: bool,

    /// Broadcast interval in seconds (for Full → Lite sync)
    pub broadcast_interval_secs: u64,

    /// Collections to sync TO Lite nodes (Full → Lite)
    /// If empty, no outbound sync. Common values:
    /// - "beacons" - Friendly force positions
    /// - "alerts" - Time-critical notifications
    /// - "commands" - Issued commands for this node
    /// - "waypoints" - Navigation points
    pub outbound_collections: Vec<String>,

    /// Collections to accept FROM Lite nodes (Lite → Full)
    /// If empty, accepts all. Common values:
    /// - "lite_sensors" - Sensor readings (temp, accel, etc.)
    /// - "lite_events" - Button presses, detections
    /// - "lite_status" - Battery, health, etc.
    pub inbound_collections: Vec<String>,

    /// Maximum document age (seconds) to sync to Lite nodes
    /// Older documents are skipped to save bandwidth
    /// 0 = no age limit
    pub max_document_age_secs: u64,

    /// Sync mode for outbound data
    pub outbound_sync_mode: LiteSyncMode,
}

/// Sync mode for Full → Lite communication
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiteSyncMode {
    /// Only send latest state (no history)
    #[default]
    LatestOnly,

    /// Send deltas since last sync
    DeltaSync,

    /// No outbound sync (receive only)
    ReceiveOnly,
}

impl Default for LiteTransportConfig {
    fn default() -> Self {
        Self {
            listen_port: DEFAULT_PORT,
            broadcast_port: DEFAULT_PORT,
            peer_timeout_secs: 30,
            enable_broadcast: true,
            broadcast_interval_secs: 2,
            // Default: accept sensor data from Lite, send alerts/beacons to Lite
            outbound_collections: vec!["beacons".to_string(), "alerts".to_string()],
            inbound_collections: vec![
                "lite_sensors".to_string(),
                "lite_events".to_string(),
                "lite_status".to_string(),
            ],
            max_document_age_secs: 300, // 5 minutes
            outbound_sync_mode: LiteSyncMode::LatestOnly,
        }
    }
}

// =============================================================================
// Lite Peer State
// =============================================================================

/// State tracked for each connected Lite peer
#[derive(Debug, Clone)]
pub struct LitePeerState {
    /// Node ID (u32 from wire protocol)
    pub node_id_raw: u32,

    /// Last known address
    pub address: SocketAddr,

    /// Capabilities announced by peer
    pub capabilities: LiteCapabilities,

    /// Last heartbeat received
    pub last_seen: Instant,

    /// Last sequence number received
    pub last_seq: u32,

    /// When connection was established
    pub connected_at: Instant,

    /// Message count received
    pub message_count: u64,
}

// =============================================================================
// Lite Connection
// =============================================================================

/// Virtual connection to an Peat-Lite peer
pub struct LiteConnection {
    node_id: NodeId,
    state: Arc<std::sync::RwLock<LitePeerState>>,
    connected_at: Instant,
}

impl MeshConnection for LiteConnection {
    fn peer_id(&self) -> &NodeId {
        &self.node_id
    }

    fn is_alive(&self) -> bool {
        // Check if we've received a heartbeat recently
        if let Ok(state) = self.state.read() {
            state.last_seen.elapsed() < Duration::from_secs(30)
        } else {
            true // Assume alive if we can't check
        }
    }

    fn connected_at(&self) -> Instant {
        self.connected_at
    }
}

// =============================================================================
// Lite Mesh Transport
// =============================================================================

/// Transport for communicating with Peat-Lite nodes
pub struct LiteMeshTransport {
    config: LiteTransportConfig,

    /// Connected Lite peers indexed by NodeId string
    /// Using std::sync::RwLock for sync trait method access
    peers: Arc<std::sync::RwLock<HashMap<String, Arc<std::sync::RwLock<LitePeerState>>>>>,

    /// UDP socket for sending/receiving
    socket: Arc<Mutex<Option<Arc<UdpSocket>>>>,

    /// Running flag
    running: Arc<std::sync::RwLock<bool>>,

    /// Event senders for peer notifications
    /// Using std::sync::Mutex for sync trait method access
    event_senders: Arc<StdMutex<Vec<PeerEventSender>>>,

    /// Our node ID (for outgoing messages)
    pub local_node_id: u32,

    /// Sequence number for outgoing messages
    pub seq_num: Arc<Mutex<u32>>,

    /// Callback for received CRDT data
    /// (collection, doc_id, crdt_type, crdt_data)
    crdt_callback: CrdtCallback,

    /// Callback for query requests
    query_callback: QueryCallback,

    /// Callback for OTA messages (peer_id, msg_type, payload)
    ota_callback: OtaCallback,
}

impl LiteMeshTransport {
    /// Create a new Peat-Lite transport
    pub fn new(config: LiteTransportConfig, local_node_id: u32) -> Self {
        Self {
            config,
            peers: Arc::new(std::sync::RwLock::new(HashMap::new())),
            socket: Arc::new(Mutex::new(None)),
            running: Arc::new(std::sync::RwLock::new(false)),
            event_senders: Arc::new(StdMutex::new(Vec::new())),
            local_node_id,
            seq_num: Arc::new(Mutex::new(0)),
            crdt_callback: Arc::new(StdMutex::new(None)),
            query_callback: Arc::new(StdMutex::new(None)),
            ota_callback: Arc::new(StdMutex::new(None)),
        }
    }

    /// Set callback for received CRDT data
    ///
    /// The callback receives (collection, doc_id, crdt_type, crdt_data)
    pub fn set_crdt_callback<F>(&self, callback: F)
    where
        F: Fn(&str, &str, CrdtType, &[u8]) + Send + Sync + 'static,
    {
        let mut cb = self.crdt_callback.lock().unwrap_or_else(|e| e.into_inner());
        *cb = Some(Box::new(callback));
    }

    /// Set callback for query requests
    ///
    /// The callback receives a QueryRequest and should return response bytes if
    /// the query can be answered, or None otherwise.
    pub fn set_query_callback<F>(&self, callback: F)
    where
        F: Fn(&QueryRequest) -> Option<Vec<u8>> + Send + Sync + 'static,
    {
        let mut cb = self
            .query_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *cb = Some(Box::new(callback));
    }

    /// Set callback for OTA messages from Lite peers.
    ///
    /// The callback receives (peer_id, message_type, payload).
    pub fn set_ota_callback<F>(&self, callback: F)
    where
        F: Fn(&str, MessageType, &[u8]) + Send + Sync + 'static,
    {
        let mut cb = self.ota_callback.lock().unwrap_or_else(|e| e.into_inner());
        *cb = Some(Box::new(callback));
    }

    /// Broadcast a message to all Lite peers
    pub async fn broadcast(&self, msg: &LiteMessage) -> Result<()> {
        let socket_guard = self.socket.lock().await;
        let socket = socket_guard.as_ref().ok_or(TransportError::NotStarted)?;

        let data = msg.encode();
        let broadcast_addr = format!("255.255.255.255:{}", self.config.broadcast_port);

        socket
            .send_to(&data, &broadcast_addr)
            .await
            .map_err(|e| TransportError::Other(Box::new(e)))?;

        Ok(())
    }

    /// Send a message to a specific Lite peer
    pub async fn send_to(&self, peer_id: &NodeId, msg: &LiteMessage) -> Result<()> {
        let socket_guard = self.socket.lock().await;
        let socket = socket_guard.as_ref().ok_or(TransportError::NotStarted)?;

        let addr = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            let peer_state = peers
                .get(peer_id.as_str())
                .ok_or_else(|| TransportError::PeerNotFound(peer_id.to_string()))?;
            let addr = peer_state.read().unwrap_or_else(|e| e.into_inner()).address;
            addr
        };

        let data = msg.encode();

        socket
            .send_to(&data, addr)
            .await
            .map_err(|e| TransportError::Other(Box::new(e)))?;

        Ok(())
    }

    /// Send peer event to all subscribers
    fn send_event(&self, event: PeerEvent) {
        let senders = self.event_senders.lock().unwrap_or_else(|e| e.into_inner());
        for sender in senders.iter() {
            let _ = sender.try_send(event.clone());
        }
    }

    /// Handle incoming message
    fn handle_message(&self, msg: LiteMessage, src: SocketAddr) {
        // Ignore messages from ourselves (received via broadcast loopback)
        if msg.node_id == self.local_node_id {
            return;
        }

        let node_id_str = format!("lite-{:08X}", msg.node_id);
        let node_id = NodeId::new(node_id_str.clone());

        // Update or create peer state
        let is_new_peer = {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());

            if let Some(peer_state) = peers.get(&node_id_str) {
                // Update existing peer
                let mut state = peer_state.write().unwrap_or_else(|e| e.into_inner());
                state.last_seen = Instant::now();
                state.last_seq = msg.seq_num;
                state.address = src;
                state.message_count += 1;

                // Update capabilities from ANNOUNCE
                if msg.msg_type == MessageType::Announce && !msg.payload.is_empty() {
                    state.capabilities = LiteCapabilities::from_bytes(&msg.payload);
                }

                false
            } else {
                // New peer
                let capabilities =
                    if msg.msg_type == MessageType::Announce && !msg.payload.is_empty() {
                        LiteCapabilities::from_bytes(&msg.payload)
                    } else {
                        LiteCapabilities::default()
                    };

                let state = LitePeerState {
                    node_id_raw: msg.node_id,
                    address: src,
                    capabilities,
                    last_seen: Instant::now(),
                    last_seq: msg.seq_num,
                    connected_at: Instant::now(),
                    message_count: 1,
                };

                peers.insert(node_id_str.clone(), Arc::new(std::sync::RwLock::new(state)));
                true
            }
        };

        // Emit connected event for new peers
        if is_new_peer {
            log::info!("Lite peer connected: {} from {}", node_id_str, src);
            self.send_event(PeerEvent::Connected {
                peer_id: node_id.clone(),
                connected_at: Instant::now(),
            });
        }

        // Handle message by type
        match msg.msg_type {
            MessageType::Announce => {
                log::debug!("ANNOUNCE from {} caps=0x{:04X}", node_id_str, msg.flags);
            }
            MessageType::Heartbeat => {
                log::trace!("HEARTBEAT from {} seq={}", node_id_str, msg.seq_num);
            }
            MessageType::Data => {
                if !msg.payload.is_empty() {
                    if let Some(crdt_type) = CrdtType::from_u8(msg.payload[0]) {
                        let crdt_data = &msg.payload[1..];

                        log::debug!(
                            "DATA from {} type={:?} len={}",
                            node_id_str,
                            crdt_type,
                            crdt_data.len()
                        );

                        // Call CRDT callback if set
                        if let Some(callback) = self
                            .crdt_callback
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                        {
                            // Use node_id as doc_id, "lite_sensors" as collection
                            callback("lite_sensors", &node_id_str, crdt_type, crdt_data);
                        }
                    }
                }
            }
            MessageType::Leave => {
                log::info!("LEAVE from {}", node_id_str);
                // Remove peer and emit disconnected event
                let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
                if let Some(peer_state) = peers.remove(&node_id_str) {
                    let state = peer_state.read().unwrap_or_else(|e| e.into_inner());
                    self.send_event(PeerEvent::Disconnected {
                        peer_id: node_id,
                        reason: DisconnectReason::RemoteClosed,
                        connection_duration: state.connected_at.elapsed(),
                    });
                }
            }
            MessageType::Query => {
                if let Some(request) = QueryRequest::decode(&msg.payload) {
                    log::debug!(
                        "QUERY from {} type={:?} collection={}",
                        node_id_str,
                        request.crdt_type,
                        request.collection,
                    );

                    if let Some(callback) = self
                        .query_callback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                    {
                        if let Some(_response) = callback(&request) {
                            // Response would be sent back via send_to in a real async context.
                            // For now, the callback handles the response externally.
                            log::debug!("Query response ready for {}", node_id_str);
                        }
                    }
                }
            }
            // OTA messages from Lite peers → dispatch to OtaSender via callback
            MessageType::OtaAccept
            | MessageType::OtaAck
            | MessageType::OtaResult
            | MessageType::OtaAbort => {
                log::debug!("OTA message {:?} from {}", msg.msg_type, node_id_str);
                if let Some(callback) = self
                    .ota_callback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    callback(&node_id_str, msg.msg_type, &msg.payload);
                }
            }
            _ => {
                log::trace!(
                    "Unhandled message type {:?} from {}",
                    msg.msg_type,
                    node_id_str
                );
            }
        }
    }

    /// Check for stale peers and emit disconnect events
    fn check_stale_peers(&self) {
        let timeout = Duration::from_secs(self.config.peer_timeout_secs);
        let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());

        let mut stale_peers = Vec::new();
        for (id, state) in peers.iter() {
            let state = state.read().unwrap_or_else(|e| e.into_inner());
            if state.last_seen.elapsed() > timeout {
                stale_peers.push((id.clone(), state.connected_at.elapsed()));
            }
        }

        for (id, duration) in stale_peers {
            peers.remove(&id);
            log::info!("Lite peer timed out: {}", id);
            self.send_event(PeerEvent::Disconnected {
                peer_id: NodeId::new(id),
                reason: DisconnectReason::Timeout,
                connection_duration: duration,
            });
        }
    }
}

#[async_trait]
impl MeshTransport for LiteMeshTransport {
    async fn start(&self) -> Result<()> {
        // Bind UDP socket
        let addr = format!("0.0.0.0:{}", self.config.listen_port);
        let socket = UdpSocket::bind(&addr)
            .await
            .map_err(|e| TransportError::Other(Box::new(e)))?;

        // Enable broadcast
        socket
            .set_broadcast(true)
            .map_err(|e| TransportError::Other(Box::new(e)))?;

        let socket = Arc::new(socket);

        {
            let mut socket_guard = self.socket.lock().await;
            *socket_guard = Some(socket.clone());
        }

        {
            let mut running = self.running.write().unwrap_or_else(|e| e.into_inner());
            *running = true;
        }

        log::info!("LiteMeshTransport started on {}", addr);

        // Spawn receive loop
        let peers = self.peers.clone();
        let running = self.running.clone();
        let event_senders = self.event_senders.clone();
        let crdt_callback = self.crdt_callback.clone();
        let query_callback = self.query_callback.clone();
        let ota_callback = self.ota_callback.clone();
        let _config = self.config.clone();
        let transport = Self {
            config: self.config.clone(),
            peers: peers.clone(),
            socket: Arc::new(Mutex::new(Some(socket.clone()))),
            running: running.clone(),
            event_senders: event_senders.clone(),
            local_node_id: self.local_node_id,
            seq_num: self.seq_num.clone(),
            crdt_callback: crdt_callback.clone(),
            query_callback: query_callback.clone(),
            ota_callback: ota_callback.clone(),
        };

        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let mut last_stale_check = Instant::now();

            loop {
                // Check if still running
                if !*running.read().unwrap_or_else(|e| e.into_inner()) {
                    break;
                }

                // Receive with timeout
                let recv_result =
                    tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf))
                        .await;

                match recv_result {
                    Ok(Ok((len, src))) => {
                        if let Some(msg) = LiteMessage::decode(&buf[..len]) {
                            transport.handle_message(msg, src);
                        }
                    }
                    Ok(Err(e)) => {
                        log::warn!("UDP receive error: {}", e);
                    }
                    Err(_) => {
                        // Timeout - check for stale peers
                    }
                }

                // Periodically check for stale peers
                if last_stale_check.elapsed() > Duration::from_secs(5) {
                    transport.check_stale_peers();
                    last_stale_check = Instant::now();
                }
            }

            log::info!("LiteMeshTransport receive loop stopped");
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        {
            let mut running = self.running.write().unwrap_or_else(|e| e.into_inner());
            *running = false;
        }

        // Clear socket
        {
            let mut socket_guard = self.socket.lock().await;
            *socket_guard = None;
        }

        log::info!("LiteMeshTransport stopped");
        Ok(())
    }

    async fn connect(&self, peer_id: &NodeId) -> Result<Box<dyn MeshConnection>> {
        // For Lite transport, connections are created implicitly when we receive messages
        // This method can be used to "expect" a connection from a known peer

        let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = peers.get(peer_id.as_str()) {
            let state_clone = state.clone();
            let connected_at = state.read().unwrap_or_else(|e| e.into_inner()).connected_at;
            Ok(Box::new(LiteConnection {
                node_id: peer_id.clone(),
                state: state_clone,
                connected_at,
            }))
        } else {
            Err(TransportError::PeerNotFound(peer_id.to_string()))
        }
    }

    async fn disconnect(&self, peer_id: &NodeId) -> Result<()> {
        let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = peers.remove(peer_id.as_str()) {
            let state = state.read().unwrap_or_else(|e| e.into_inner());
            self.send_event(PeerEvent::Disconnected {
                peer_id: peer_id.clone(),
                reason: DisconnectReason::LocalClosed,
                connection_duration: state.connected_at.elapsed(),
            });
            Ok(())
        } else {
            Err(TransportError::PeerNotFound(peer_id.to_string()))
        }
    }

    fn get_connection(&self, peer_id: &NodeId) -> Option<Box<dyn MeshConnection>> {
        let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
        peers.get(peer_id.as_str()).map(|state| {
            let connected_at = state.read().unwrap_or_else(|e| e.into_inner()).connected_at;
            Box::new(LiteConnection {
                node_id: peer_id.clone(),
                state: state.clone(),
                connected_at,
            }) as Box<dyn MeshConnection>
        })
    }

    fn peer_count(&self) -> usize {
        self.peers.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn connected_peers(&self) -> Vec<NodeId> {
        self.peers
            .read()
            .unwrap()
            .keys()
            .map(|k| NodeId::new(k.clone()))
            .collect()
    }

    async fn send_to(&self, peer_id: &NodeId, data: &[u8]) -> Result<usize> {
        let socket_guard = self.socket.lock().await;
        let socket = socket_guard.as_ref().ok_or(TransportError::NotStarted)?;

        let addr = {
            let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
            match peers.get(peer_id.as_str()) {
                Some(peer_state) => {
                    let addr = peer_state.read().unwrap_or_else(|e| e.into_inner()).address;
                    addr
                }
                None => return Err(TransportError::PeerNotFound(peer_id.to_string())),
            }
        };

        let sent = socket
            .send_to(data, addr)
            .await
            .map_err(|e| TransportError::Other(Box::new(e)))?;

        Ok(sent)
    }

    fn subscribe_peer_events(&self) -> PeerEventReceiver {
        let (tx, rx) = mpsc::channel(PEER_EVENT_CHANNEL_CAPACITY);
        self.event_senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    fn get_peer_health(&self, peer_id: &NodeId) -> Option<ConnectionHealth> {
        let peers = self.peers.read().unwrap_or_else(|e| e.into_inner());
        peers.get(peer_id.as_str()).map(|state| {
            let state = state.read().unwrap_or_else(|e| e.into_inner());
            ConnectionHealth {
                rtt_ms: 0, // UDP doesn't track RTT
                rtt_variance_ms: 0,
                packet_loss_percent: 0,
                state: if state.last_seen.elapsed() < Duration::from_secs(10) {
                    ConnectionState::Healthy
                } else if state.last_seen.elapsed() < Duration::from_secs(30) {
                    ConnectionState::Degraded
                } else {
                    ConnectionState::Dead
                },
                last_activity: state.last_seen,
            }
        })
    }
}

// =============================================================================
// DocumentStore Integration
// =============================================================================

/// Integrates LiteMeshTransport with a DocumentStore
///
/// This struct handles:
/// - Translating incoming primitive CRDTs to Document upserts
/// - Observing DocumentStore changes and syncing to Lite nodes
/// - Collection filtering per configuration
pub struct LiteDocumentBridge {
    transport: Arc<LiteMeshTransport>,
    config: LiteTransportConfig,
}

impl LiteDocumentBridge {
    /// Create a new bridge between transport and document store
    pub fn new(transport: Arc<LiteMeshTransport>, config: LiteTransportConfig) -> Self {
        Self { transport, config }
    }

    /// Check if a collection should be accepted from Lite nodes
    pub fn accepts_inbound(&self, collection: &str) -> bool {
        self.config.inbound_collections.is_empty()
            || self
                .config
                .inbound_collections
                .iter()
                .any(|c| c == collection)
    }

    /// Check if a collection should be sent to Lite nodes
    pub fn sends_outbound(&self, collection: &str) -> bool {
        self.config
            .outbound_collections
            .iter()
            .any(|c| c == collection)
    }

    /// Decode a GCounter from wire format and return (node_counts, total)
    pub fn decode_gcounter(data: &[u8]) -> Option<(Vec<(u32, u64)>, u64)> {
        if data.len() < 6 {
            return None;
        }

        let _local_node_id = u32::from_le_bytes(data[0..4].try_into().ok()?);
        let num_entries = u16::from_le_bytes(data[4..6].try_into().ok()?) as usize;

        if data.len() < 6 + (num_entries * 12) {
            return None;
        }

        let mut counts = Vec::with_capacity(num_entries);
        let mut total = 0u64;
        let mut offset = 6;

        for _ in 0..num_entries {
            let node_id = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
            let count = u64::from_le_bytes(data[offset + 4..offset + 12].try_into().ok()?);
            counts.push((node_id, count));
            total += count;
            offset += 12;
        }

        Some((counts, total))
    }

    /// Decode an LWW-Register from wire format
    /// Returns (timestamp, node_id, value_bytes)
    pub fn decode_lww_register(data: &[u8]) -> Option<(u64, u32, Vec<u8>)> {
        if data.len() < 12 {
            return None;
        }

        let timestamp = u64::from_le_bytes(data[0..8].try_into().ok()?);
        let node_id = u32::from_le_bytes(data[8..12].try_into().ok()?);
        let value = data[12..].to_vec();

        Some((timestamp, node_id, value))
    }

    /// Convert a GCounter to Document fields
    pub fn gcounter_to_fields(
        node_id: &str,
        counts: &[(u32, u64)],
        total: u64,
    ) -> std::collections::HashMap<String, serde_json::Value> {
        let mut fields = std::collections::HashMap::new();

        fields.insert("type".to_string(), serde_json::json!("gcounter"));
        fields.insert("source_node".to_string(), serde_json::json!(node_id));
        fields.insert("total".to_string(), serde_json::json!(total));

        // Store per-node counts as nested object
        let node_counts: std::collections::HashMap<String, u64> = counts
            .iter()
            .map(|(nid, count)| (format!("{:08X}", nid), *count))
            .collect();
        fields.insert("node_counts".to_string(), serde_json::json!(node_counts));

        fields.insert(
            "updated_at".to_string(),
            serde_json::json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64),
        );

        fields
    }

    /// Convert an LWW-Register to Document fields
    pub fn lww_register_to_fields(
        node_id: &str,
        timestamp: u64,
        value: &[u8],
    ) -> std::collections::HashMap<String, serde_json::Value> {
        let mut fields = std::collections::HashMap::new();

        fields.insert("type".to_string(), serde_json::json!("lww_register"));
        fields.insert("source_node".to_string(), serde_json::json!(node_id));
        fields.insert("timestamp".to_string(), serde_json::json!(timestamp));

        // Try to interpret value as different types
        if value.len() == 4 {
            // Could be i32 or f32
            let int_val = i32::from_le_bytes(value.try_into().unwrap());
            fields.insert("value_i32".to_string(), serde_json::json!(int_val));
        } else if value.len() == 8 {
            // Could be i64 or f64
            let int_val = i64::from_le_bytes(value.try_into().unwrap());
            fields.insert("value_i64".to_string(), serde_json::json!(int_val));
        }

        // Always include raw bytes as hex
        fields.insert(
            "value_hex".to_string(),
            serde_json::json!(hex::encode(value)),
        );

        fields.insert(
            "updated_at".to_string(),
            serde_json::json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64),
        );

        fields
    }

    /// Encode a Document as a GCounter for transmission to Lite nodes
    pub fn encode_gcounter_from_doc(
        doc: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<Vec<u8>> {
        // Extract node_counts from document
        let node_counts = doc.get("node_counts")?.as_object()?;

        let entries: Vec<(u32, u64)> = node_counts
            .iter()
            .filter_map(|(k, v)| {
                let node_id = u32::from_str_radix(k, 16).ok()?;
                let count = v.as_u64()?;
                Some((node_id, count))
            })
            .collect();

        // Encode: [local_node_id:4][num_entries:2][entries:N*12]
        let mut buf = Vec::with_capacity(6 + entries.len() * 12);

        // Use 0 as local_node_id for Full node
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());

        for (node_id, count) in entries {
            buf.extend_from_slice(&node_id.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
        }

        Some(buf)
    }

    /// Decode a PnCounter from wire format
    ///
    /// Returns (entries as (node_id, increments, decrements), net_value)
    #[allow(clippy::type_complexity)]
    pub fn decode_pncounter(data: &[u8]) -> Option<(Vec<(u32, u64, u64)>, i64)> {
        if data.len() < 6 {
            return None;
        }

        let _local_node_id = u32::from_le_bytes(data[0..4].try_into().ok()?);
        let num_entries = u16::from_le_bytes(data[4..6].try_into().ok()?) as usize;

        if data.len() < 6 + (num_entries * 20) {
            return None;
        }

        let mut entries = Vec::with_capacity(num_entries);
        let mut net_value = 0i64;
        let mut offset = 6;

        for _ in 0..num_entries {
            let node_id = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
            let increments = u64::from_le_bytes(data[offset + 4..offset + 12].try_into().ok()?);
            let decrements = u64::from_le_bytes(data[offset + 12..offset + 20].try_into().ok()?);
            entries.push((node_id, increments, decrements));
            net_value += increments as i64 - decrements as i64;
            offset += 20;
        }

        Some((entries, net_value))
    }

    /// Convert a PnCounter to Document fields
    pub fn pncounter_to_fields(
        node_id: &str,
        entries: &[(u32, u64, u64)],
        net_value: i64,
    ) -> std::collections::HashMap<String, serde_json::Value> {
        let mut fields = std::collections::HashMap::new();

        fields.insert("type".to_string(), serde_json::json!("pncounter"));
        fields.insert("source_node".to_string(), serde_json::json!(node_id));
        fields.insert("net_value".to_string(), serde_json::json!(net_value));

        let node_entries: std::collections::HashMap<String, serde_json::Value> = entries
            .iter()
            .map(|(nid, inc, dec)| {
                (
                    format!("{:08X}", nid),
                    serde_json::json!({"increments": inc, "decrements": dec}),
                )
            })
            .collect();
        fields.insert("node_entries".to_string(), serde_json::json!(node_entries));

        fields.insert(
            "updated_at".to_string(),
            serde_json::json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64),
        );

        fields
    }

    /// Encode a Document as a PnCounter for transmission to Lite nodes
    pub fn encode_pncounter_from_doc(
        doc: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<Vec<u8>> {
        let node_entries = doc.get("node_entries")?.as_object()?;

        let entries: Vec<(u32, u64, u64)> = node_entries
            .iter()
            .filter_map(|(k, v)| {
                let node_id = u32::from_str_radix(k, 16).ok()?;
                let obj = v.as_object()?;
                let increments = obj.get("increments")?.as_u64()?;
                let decrements = obj.get("decrements")?.as_u64()?;
                Some((node_id, increments, decrements))
            })
            .collect();

        let mut buf = Vec::with_capacity(6 + entries.len() * 20);
        buf.extend_from_slice(&0u32.to_le_bytes()); // local_node_id (0 for Full node)
        buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());

        for (node_id, increments, decrements) in entries {
            buf.extend_from_slice(&node_id.to_le_bytes());
            buf.extend_from_slice(&increments.to_le_bytes());
            buf.extend_from_slice(&decrements.to_le_bytes());
        }

        Some(buf)
    }

    /// Decode an OrSet from wire format
    ///
    /// Returns (local_node_id, elements)
    pub fn decode_orset(data: &[u8]) -> Option<(u32, Vec<OrSetElement>)> {
        if data.len() < 6 {
            return None;
        }

        let local_node_id = u32::from_le_bytes(data[0..4].try_into().ok()?);
        let num_elements = u16::from_le_bytes(data[4..6].try_into().ok()?) as usize;

        let mut elements = Vec::with_capacity(num_elements);
        let mut offset = 6;

        for _ in 0..num_elements {
            if data.len() < offset + 10 {
                return None;
            }

            let tag_node_id = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
            let tag_seq = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().ok()?);
            let value_len =
                u16::from_le_bytes(data[offset + 8..offset + 10].try_into().ok()?) as usize;

            if data.len() < offset + 10 + value_len {
                return None;
            }

            let value = data[offset + 10..offset + 10 + value_len].to_vec();

            elements.push(OrSetElement {
                tag_node_id,
                tag_seq,
                value,
            });

            offset += 10 + value_len;
        }

        Some((local_node_id, elements))
    }

    /// Convert an OrSet to Document fields
    pub fn orset_to_fields(
        node_id: &str,
        elements: &[OrSetElement],
    ) -> std::collections::HashMap<String, serde_json::Value> {
        let mut fields = std::collections::HashMap::new();

        fields.insert("type".to_string(), serde_json::json!("orset"));
        fields.insert("source_node".to_string(), serde_json::json!(node_id));
        fields.insert("count".to_string(), serde_json::json!(elements.len()));

        let element_values: Vec<serde_json::Value> = elements
            .iter()
            .map(|e| {
                serde_json::json!({
                    "tag_node": format!("{:08X}", e.tag_node_id),
                    "tag_seq": e.tag_seq,
                    "value": hex::encode(&e.value),
                })
            })
            .collect();
        fields.insert("elements".to_string(), serde_json::json!(element_values));

        fields.insert(
            "updated_at".to_string(),
            serde_json::json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64),
        );

        fields
    }

    /// Encode a Document as an OrSet for transmission to Lite nodes
    pub fn encode_orset_from_doc(
        doc: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<Vec<u8>> {
        let elements_arr = doc.get("elements")?.as_array()?;

        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // local_node_id (0 for Full node)
        buf.extend_from_slice(&(elements_arr.len() as u16).to_le_bytes());

        for elem in elements_arr {
            let obj = elem.as_object()?;
            let tag_node_str = obj.get("tag_node")?.as_str()?;
            let tag_node_id = u32::from_str_radix(tag_node_str, 16).ok()?;
            let tag_seq = obj.get("tag_seq")?.as_u64()? as u32;
            let value_hex = obj.get("value")?.as_str()?;
            let value = hex::decode(value_hex).ok()?;

            buf.extend_from_slice(&tag_node_id.to_le_bytes());
            buf.extend_from_slice(&tag_seq.to_le_bytes());
            buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
            buf.extend_from_slice(&value);
        }

        Some(buf)
    }

    /// Encode a generic document as LWW-Register payload
    ///
    /// Used for simple key-value data like beacons, alerts
    pub fn encode_lww_from_doc(
        doc: &std::collections::HashMap<String, serde_json::Value>,
        local_node_id: u32,
    ) -> Option<Vec<u8>> {
        // Serialize document to compact JSON
        let json = serde_json::to_vec(doc).ok()?;

        // Use current time as timestamp
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;

        let mut buf = Vec::with_capacity(12 + json.len());
        buf.extend_from_slice(&timestamp.to_le_bytes());
        buf.extend_from_slice(&local_node_id.to_le_bytes());
        buf.extend_from_slice(&json);

        Some(buf)
    }

    /// Broadcast a document to all connected Lite nodes
    ///
    /// Only sends if collection is in outbound_collections config
    pub async fn broadcast_document(
        &self,
        collection: &str,
        doc_id: &str,
        fields: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        if !self.sends_outbound(collection) {
            return Ok(()); // Filtered out
        }

        // Determine CRDT type based on document fields
        let (crdt_type, payload) = if fields.contains_key("node_entries") {
            // PnCounter document (has node_entries with increments/decrements)
            let payload = Self::encode_pncounter_from_doc(fields)
                .ok_or_else(|| TransportError::Other("Failed to encode PnCounter".into()))?;
            (CrdtType::PnCounter, payload)
        } else if fields.contains_key("elements") {
            // OrSet document
            let payload = Self::encode_orset_from_doc(fields)
                .ok_or_else(|| TransportError::Other("Failed to encode OrSet".into()))?;
            (CrdtType::OrSet, payload)
        } else if fields.contains_key("node_counts") {
            // GCounter document
            let payload = Self::encode_gcounter_from_doc(fields)
                .ok_or_else(|| TransportError::Other("Failed to encode GCounter".into()))?;
            (CrdtType::GCounter, payload)
        } else {
            // Default to LWW-Register with JSON payload
            let payload = Self::encode_lww_from_doc(fields, self.transport.local_node_id)
                .ok_or_else(|| TransportError::Other("Failed to encode LWW".into()))?;
            (CrdtType::LwwRegister, payload)
        };

        let seq = {
            let mut seq = self.transport.seq_num.lock().await;
            *seq += 1;
            *seq
        };

        let msg = LiteMessage::data(self.transport.local_node_id, seq, crdt_type, &payload);

        // Send via broadcast
        self.transport.broadcast(&msg).await?;

        // Also send unicast to all known peers (broadcast sometimes doesn't reach all devices)
        let peers = self.transport.connected_peers();
        for peer_id in &peers {
            if let Err(e) = self.transport.send_to(peer_id, &msg).await {
                log::warn!("Failed to unicast to {}: {}", peer_id, e);
            }
        }

        log::debug!(
            "Broadcast {} doc {} to {} Lite nodes ({} bytes)",
            collection,
            doc_id,
            peers.len(),
            payload.len()
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_encode_decode() {
        let msg = LiteMessage {
            msg_type: MessageType::Heartbeat,
            flags: 0,
            node_id: 0x12345678,
            seq_num: 42,
            payload: vec![],
        };

        let encoded = msg.encode();
        let decoded = LiteMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::Heartbeat);
        assert_eq!(decoded.node_id, 0x12345678);
        assert_eq!(decoded.seq_num, 42);
    }

    #[test]
    fn test_message_with_payload() {
        let msg = LiteMessage::data(0xAABBCCDD, 100, CrdtType::GCounter, &[1, 2, 3, 4]);

        let encoded = msg.encode();
        let decoded = LiteMessage::decode(&encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::Data);
        assert_eq!(decoded.payload[0], CrdtType::GCounter as u8);
        assert_eq!(&decoded.payload[1..], &[1, 2, 3, 4]);
    }

    #[test]
    fn test_invalid_magic() {
        let buf = [
            0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(LiteMessage::decode(&buf).is_none());
    }

    #[test]
    fn test_capabilities() {
        let caps = LiteCapabilities::from_bits(
            LiteCapabilities::PRIMITIVE_CRDT | LiteCapabilities::SENSOR_INPUT,
        );
        assert!(caps.has(LiteCapabilities::PRIMITIVE_CRDT));
        assert!(caps.has(LiteCapabilities::SENSOR_INPUT));
        assert!(!caps.has(LiteCapabilities::DOCUMENT_CRDT));
    }

    #[test]
    fn test_decode_gcounter() {
        // GCounter with 2 entries: node 0x11111111=5, node 0x22222222=10
        let mut data = Vec::new();
        data.extend_from_slice(&0x11111111u32.to_le_bytes()); // local node id
        data.extend_from_slice(&2u16.to_le_bytes()); // num entries
        data.extend_from_slice(&0x11111111u32.to_le_bytes()); // entry 1 node
        data.extend_from_slice(&5u64.to_le_bytes()); // entry 1 count
        data.extend_from_slice(&0x22222222u32.to_le_bytes()); // entry 2 node
        data.extend_from_slice(&10u64.to_le_bytes()); // entry 2 count

        let (counts, total) = LiteDocumentBridge::decode_gcounter(&data).unwrap();
        assert_eq!(counts.len(), 2);
        assert_eq!(counts[0], (0x11111111, 5));
        assert_eq!(counts[1], (0x22222222, 10));
        assert_eq!(total, 15);
    }

    #[test]
    fn test_decode_lww_register() {
        // LWW-Register: timestamp=1000, node=0xAABBCCDD, value="Hi"
        let mut data = Vec::new();
        data.extend_from_slice(&1000u64.to_le_bytes());
        data.extend_from_slice(&0xAABBCCDDu32.to_le_bytes());
        data.extend_from_slice(b"Hi");

        let (ts, node, value) = LiteDocumentBridge::decode_lww_register(&data).unwrap();
        assert_eq!(ts, 1000);
        assert_eq!(node, 0xAABBCCDD);
        assert_eq!(value, b"Hi");
    }

    #[test]
    fn test_gcounter_roundtrip() {
        let counts = vec![(0x11111111u32, 5u64), (0x22222222u32, 10u64)];
        let fields = LiteDocumentBridge::gcounter_to_fields("test-node", &counts, 15);

        assert_eq!(fields.get("type").unwrap(), "gcounter");
        assert_eq!(fields.get("total").unwrap(), 15);

        // Re-encode
        let encoded = LiteDocumentBridge::encode_gcounter_from_doc(&fields).unwrap();

        // Decode again (skip local_node_id which is 0 from Full node)
        let (decoded_counts, decoded_total) =
            LiteDocumentBridge::decode_gcounter(&encoded).unwrap();

        assert_eq!(decoded_total, 15);
        assert_eq!(decoded_counts.len(), 2);
    }

    #[test]
    fn test_collection_filtering() {
        let config = LiteTransportConfig {
            outbound_collections: vec!["beacons".to_string(), "alerts".to_string()],
            inbound_collections: vec!["lite_sensors".to_string()],
            ..Default::default()
        };

        // Create a mock transport (we just need the bridge for testing filtering)
        let transport = Arc::new(LiteMeshTransport::new(config.clone(), 0x12345678));
        let bridge = LiteDocumentBridge::new(transport, config);

        // Outbound checks
        assert!(bridge.sends_outbound("beacons"));
        assert!(bridge.sends_outbound("alerts"));
        assert!(!bridge.sends_outbound("squad_summaries"));

        // Inbound checks
        assert!(bridge.accepts_inbound("lite_sensors"));
        assert!(!bridge.accepts_inbound("lite_events")); // Not in list
    }

    #[tokio::test]
    async fn test_send_to_not_started() {
        let transport = LiteMeshTransport::new(LiteTransportConfig::default(), 0x12345678);
        let result =
            MeshTransport::send_to(&transport, &NodeId::new("AABBCCDD".to_string()), b"hello")
                .await;
        assert!(matches!(result, Err(TransportError::NotStarted)));
    }

    #[tokio::test]
    async fn test_send_to_unknown_peer() {
        let config = LiteTransportConfig {
            listen_port: 0, // OS-assigned port
            ..Default::default()
        };
        let transport = LiteMeshTransport::new(config, 0x12345678);
        transport.start().await.unwrap();

        let result =
            MeshTransport::send_to(&transport, &NodeId::new("AABBCCDD".to_string()), b"hello")
                .await;
        assert!(matches!(result, Err(TransportError::PeerNotFound(_))));

        transport.stop().await.unwrap();
    }

    // === PnCounter tests ===

    #[test]
    fn test_decode_pncounter() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x11111111u32.to_le_bytes()); // local node id
        data.extend_from_slice(&2u16.to_le_bytes()); // num entries
                                                     // Entry 1: node=0x11, inc=10, dec=3
        data.extend_from_slice(&0x11111111u32.to_le_bytes());
        data.extend_from_slice(&10u64.to_le_bytes());
        data.extend_from_slice(&3u64.to_le_bytes());
        // Entry 2: node=0x22, inc=5, dec=1
        data.extend_from_slice(&0x22222222u32.to_le_bytes());
        data.extend_from_slice(&5u64.to_le_bytes());
        data.extend_from_slice(&1u64.to_le_bytes());

        let (entries, net) = LiteDocumentBridge::decode_pncounter(&data).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (0x11111111, 10, 3));
        assert_eq!(entries[1], (0x22222222, 5, 1));
        assert_eq!(net, 11); // (10-3) + (5-1)
    }

    #[test]
    fn test_pncounter_to_fields() {
        let entries = vec![(0x11111111u32, 10u64, 3u64), (0x22222222u32, 5u64, 1u64)];
        let fields = LiteDocumentBridge::pncounter_to_fields("test-node", &entries, 11);

        assert_eq!(fields.get("type").unwrap(), "pncounter");
        assert_eq!(fields.get("net_value").unwrap(), 11);
        assert!(fields.contains_key("node_entries"));
    }

    #[test]
    fn test_pncounter_roundtrip() {
        let entries = vec![(0x11111111u32, 10u64, 3u64), (0x22222222u32, 5u64, 1u64)];
        let fields = LiteDocumentBridge::pncounter_to_fields("test-node", &entries, 11);

        let encoded = LiteDocumentBridge::encode_pncounter_from_doc(&fields).unwrap();
        let (decoded_entries, decoded_net) =
            LiteDocumentBridge::decode_pncounter(&encoded).unwrap();

        assert_eq!(decoded_net, 11);
        assert_eq!(decoded_entries.len(), 2);
    }

    #[test]
    fn test_pncounter_short_buffer() {
        // Too short for header
        assert!(LiteDocumentBridge::decode_pncounter(&[0; 5]).is_none());

        // Header says 1 entry but buffer is too short
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        assert!(LiteDocumentBridge::decode_pncounter(&data).is_none());
    }

    // === OrSet tests ===

    #[test]
    fn test_decode_orset() {
        let mut data = Vec::new();
        data.extend_from_slice(&0xAABBCCDDu32.to_le_bytes()); // local node id
        data.extend_from_slice(&2u16.to_le_bytes()); // num elements
                                                     // Element 1: tag_node=0x11, tag_seq=1, value="hi"
        data.extend_from_slice(&0x11111111u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(b"hi");
        // Element 2: tag_node=0x22, tag_seq=2, value="bye"
        data.extend_from_slice(&0x22222222u32.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&3u16.to_le_bytes());
        data.extend_from_slice(b"bye");

        let (local_id, elements) = LiteDocumentBridge::decode_orset(&data).unwrap();
        assert_eq!(local_id, 0xAABBCCDD);
        assert_eq!(elements.len(), 2);
        assert_eq!(
            elements[0],
            OrSetElement {
                tag_node_id: 0x11111111,
                tag_seq: 1,
                value: b"hi".to_vec(),
            }
        );
        assert_eq!(
            elements[1],
            OrSetElement {
                tag_node_id: 0x22222222,
                tag_seq: 2,
                value: b"bye".to_vec(),
            }
        );
    }

    #[test]
    fn test_orset_to_fields() {
        let elements = vec![
            OrSetElement {
                tag_node_id: 0x11111111,
                tag_seq: 1,
                value: b"hi".to_vec(),
            },
            OrSetElement {
                tag_node_id: 0x22222222,
                tag_seq: 2,
                value: b"bye".to_vec(),
            },
        ];

        let fields = LiteDocumentBridge::orset_to_fields("test-node", &elements);
        assert_eq!(fields.get("type").unwrap(), "orset");
        assert_eq!(fields.get("count").unwrap(), 2);
        assert!(fields.contains_key("elements"));
    }

    #[test]
    fn test_orset_roundtrip() {
        let elements = vec![
            OrSetElement {
                tag_node_id: 0x11111111,
                tag_seq: 1,
                value: b"hi".to_vec(),
            },
            OrSetElement {
                tag_node_id: 0x22222222,
                tag_seq: 2,
                value: b"bye".to_vec(),
            },
        ];

        let fields = LiteDocumentBridge::orset_to_fields("test-node", &elements);
        let encoded = LiteDocumentBridge::encode_orset_from_doc(&fields).unwrap();
        let (_local_id, decoded_elements) = LiteDocumentBridge::decode_orset(&encoded).unwrap();

        assert_eq!(decoded_elements.len(), 2);
        assert_eq!(decoded_elements[0].value, b"hi");
        assert_eq!(decoded_elements[1].value, b"bye");
    }

    #[test]
    fn test_orset_empty_set() {
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u16.to_le_bytes());

        let (_, elements) = LiteDocumentBridge::decode_orset(&data).unwrap();
        assert!(elements.is_empty());
    }

    #[test]
    fn test_orset_short_buffer() {
        assert!(LiteDocumentBridge::decode_orset(&[0; 5]).is_none());

        // Header says 1 element but buffer is too short
        let mut data = Vec::new();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        assert!(LiteDocumentBridge::decode_orset(&data).is_none());
    }

    // === Query tests ===

    #[test]
    fn test_query_request_encode_decode() {
        let request = QueryRequest {
            crdt_type: CrdtType::LwwRegister,
            collection: "sensors".to_string(),
            doc_id: Some("temp-01".to_string()),
        };

        let encoded = request.encode();
        let decoded = QueryRequest::decode(&encoded).unwrap();

        assert_eq!(decoded.crdt_type, CrdtType::LwwRegister);
        assert_eq!(decoded.collection, "sensors");
        assert_eq!(decoded.doc_id, Some("temp-01".to_string()));
    }

    #[test]
    fn test_query_request_no_doc_id() {
        let request = QueryRequest {
            crdt_type: CrdtType::GCounter,
            collection: "counters".to_string(),
            doc_id: None,
        };

        let encoded = request.encode();
        let decoded = QueryRequest::decode(&encoded).unwrap();

        assert_eq!(decoded.crdt_type, CrdtType::GCounter);
        assert_eq!(decoded.collection, "counters");
        assert!(decoded.doc_id.is_none());
    }

    #[test]
    fn test_query_request_short_buffer() {
        assert!(QueryRequest::decode(&[0; 2]).is_none());
        // Invalid CRDT type
        assert!(QueryRequest::decode(&[0xFF, 0, 0]).is_none());
    }

    #[test]
    fn test_query_message_construction() {
        let request = QueryRequest {
            crdt_type: CrdtType::OrSet,
            collection: "tags".to_string(),
            doc_id: None,
        };

        let msg = LiteMessage::query(0x12345678, 42, &request);
        assert_eq!(msg.msg_type, MessageType::Query);
        assert_eq!(msg.node_id, 0x12345678);
        assert_eq!(msg.seq_num, 42);

        // Decode the payload
        let decoded_req = QueryRequest::decode(&msg.payload).unwrap();
        assert_eq!(decoded_req.crdt_type, CrdtType::OrSet);
        assert_eq!(decoded_req.collection, "tags");
    }

    #[test]
    fn test_handle_query_message() {
        let transport = LiteMeshTransport::new(LiteTransportConfig::default(), 0x12345678);

        let called = Arc::new(StdMutex::new(false));
        let called_clone = called.clone();

        transport.set_query_callback(move |req: &QueryRequest| {
            *called_clone.lock().unwrap_or_else(|e| e.into_inner()) = true;
            assert_eq!(req.collection, "sensors");
            Some(vec![1, 2, 3])
        });

        // Build a query message from a different node
        let request = QueryRequest {
            crdt_type: CrdtType::LwwRegister,
            collection: "sensors".to_string(),
            doc_id: None,
        };
        let msg = LiteMessage::query(0xAABBCCDD, 1, &request);

        let src_addr: SocketAddr = "192.168.1.100:5555".parse().unwrap();
        transport.handle_message(msg, src_addr);

        assert!(*called.lock().unwrap_or_else(|e| e.into_inner()));
    }
}
