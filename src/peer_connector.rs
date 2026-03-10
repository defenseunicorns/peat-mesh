//! Bridges discovery events to Iroh peer management.
//!
//! The `PeerConnector` consumes `DiscoveryEvent`s (from Kubernetes, mDNS, etc.)
//! and teaches the Iroh endpoint about discovered peers via the `StaticProvider`.
//! This enables Iroh QUIC connections to peers whose addresses were learned
//! through non-Iroh discovery mechanisms.

use crate::discovery::DiscoveryEvent;
use crate::security::certificate::CertificateBundle;
use crate::storage::NetworkedIrohBlobStore;
use hkdf::Hkdf;
use iroh::{EndpointAddr, EndpointId, SecretKey, TransportAddr};
use sha2::Sha256;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Bridges discovery events to Iroh's networking layer.
///
/// Given a formation secret, the connector derives deterministic Iroh
/// `EndpointId`s for discovered peers (using HKDF with context `"iroh:" + hostname`)
/// and registers their addresses with the blob store's `StaticProvider`.
pub struct PeerConnector {
    formation_secret: Vec<u8>,
    blob_store: Arc<NetworkedIrohBlobStore>,
    /// Optional certificate bundle for peer validation.
    /// When set, only peers with valid certificates are connected.
    certificate_bundle: Option<Arc<RwLock<CertificateBundle>>>,
    /// Whether to require peer certificates. When false and a bundle is present,
    /// unknown peers are allowed with a warning.
    require_certificates: bool,
}

impl PeerConnector {
    /// Create a new `PeerConnector`.
    ///
    /// # Arguments
    ///
    /// * `formation_secret` - Shared secret (raw bytes, already base64-decoded)
    /// * `blob_store` - The networked blob store whose StaticProvider and peer list to update
    pub fn new(formation_secret: Vec<u8>, blob_store: Arc<NetworkedIrohBlobStore>) -> Self {
        Self {
            formation_secret,
            blob_store,
            certificate_bundle: None,
            require_certificates: false,
        }
    }

    /// Set the certificate bundle for peer validation.
    pub fn with_certificate_bundle(
        mut self,
        bundle: Arc<RwLock<CertificateBundle>>,
        require: bool,
    ) -> Self {
        self.certificate_bundle = Some(bundle);
        self.require_certificates = require;
        self
    }

    /// Derive the Iroh `EndpointId` for a peer given its hostname.
    ///
    /// Uses `HKDF(formation_secret, "iroh:" + hostname)` → `SecretKey` → `.public()`.
    /// This is deterministic: any node with the same formation secret can compute
    /// any other node's `EndpointId` from its hostname alone.
    pub fn derive_peer_endpoint_id(&self, hostname: &str) -> EndpointId {
        let hk = Hkdf::<Sha256>::new(None, &self.formation_secret);
        let mut okm = [0u8; 32];
        let context = format!("iroh:{}", hostname);
        hk.expand(context.as_bytes(), &mut okm)
            .expect("HKDF expand with 32-byte output should never fail");
        SecretKey::from_bytes(&okm).public()
    }

    /// Run the connector, consuming discovery events and updating Iroh.
    ///
    /// Returns a `JoinHandle` for the spawned background task.
    pub fn run(self, mut events: mpsc::Receiver<DiscoveryEvent>) -> JoinHandle<()> {
        tokio::spawn(async move {
            info!("PeerConnector started");

            let my_endpoint_id = self.blob_store.endpoint_id();

            while let Some(event) = events.recv().await {
                match event {
                    DiscoveryEvent::PeerFound(peer_info) => {
                        let endpoint_id = self.derive_peer_endpoint_id(&peer_info.node_id);

                        // Don't connect to ourselves
                        if endpoint_id == my_endpoint_id {
                            debug!(
                                peer = %peer_info.node_id,
                                "Skipping self in peer discovery"
                            );
                            continue;
                        }

                        // Certificate validation
                        if let Some(ref bundle) = self.certificate_bundle {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;
                            let bundle = bundle.read().unwrap_or_else(|e| e.into_inner());
                            if !bundle.validate_node_id(&peer_info.node_id, now) {
                                if self.require_certificates {
                                    warn!(
                                        peer = %peer_info.node_id,
                                        "Rejecting peer: no valid certificate"
                                    );
                                    continue;
                                }
                                debug!(
                                    peer = %peer_info.node_id,
                                    "Peer has no certificate (not required)"
                                );
                            } else {
                                let tier = bundle.get_node_tier(&peer_info.node_id);
                                info!(
                                    peer = %peer_info.node_id,
                                    tier = ?tier,
                                    "Peer certificate validated"
                                );
                            }
                        }

                        let addrs: std::collections::BTreeSet<TransportAddr> = peer_info
                            .addresses
                            .iter()
                            .map(|a| TransportAddr::Ip(*a))
                            .collect();

                        let endpoint_addr = EndpointAddr {
                            id: endpoint_id,
                            addrs,
                        };

                        self.blob_store
                            .static_provider()
                            .add_endpoint_info(endpoint_addr);
                        self.blob_store.add_peer(endpoint_id).await;

                        info!(
                            peer = %peer_info.node_id,
                            endpoint_id = %endpoint_id.fmt_short(),
                            addresses = ?peer_info.addresses,
                            "Peer connected to Iroh"
                        );
                    }
                    DiscoveryEvent::PeerLost(node_id) => {
                        let endpoint_id = self.derive_peer_endpoint_id(&node_id);

                        self.blob_store
                            .static_provider()
                            .remove_endpoint_info(endpoint_id);
                        self.blob_store.remove_peer(&endpoint_id).await;

                        info!(
                            peer = %node_id,
                            endpoint_id = %endpoint_id.fmt_short(),
                            "Peer removed from Iroh"
                        );
                    }
                    DiscoveryEvent::PeerUpdated(peer_info) => {
                        let endpoint_id = self.derive_peer_endpoint_id(&peer_info.node_id);

                        if endpoint_id == my_endpoint_id {
                            continue;
                        }

                        // Re-validate certificate on update
                        let should_remove = if let Some(ref bundle) = self.certificate_bundle {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;
                            let bundle = bundle.read().unwrap_or_else(|e| e.into_inner());
                            !bundle.validate_node_id(&peer_info.node_id, now)
                                && self.require_certificates
                        } else {
                            false
                        };
                        if should_remove {
                            warn!(
                                peer = %peer_info.node_id,
                                "Removing peer on update: certificate no longer valid"
                            );
                            self.blob_store
                                .static_provider()
                                .remove_endpoint_info(endpoint_id);
                            self.blob_store.remove_peer(&endpoint_id).await;
                            continue;
                        }

                        let addrs: std::collections::BTreeSet<TransportAddr> = peer_info
                            .addresses
                            .iter()
                            .map(|a| TransportAddr::Ip(*a))
                            .collect();

                        let endpoint_addr = EndpointAddr {
                            id: endpoint_id,
                            addrs,
                        };

                        self.blob_store
                            .static_provider()
                            .set_endpoint_info(endpoint_addr);

                        debug!(
                            peer = %peer_info.node_id,
                            endpoint_id = %endpoint_id.fmt_short(),
                            addresses = ?peer_info.addresses,
                            "Peer addresses updated in Iroh"
                        );
                    }
                }
            }

            warn!("PeerConnector event stream closed");
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that key derivation is deterministic: same inputs → same output.
    #[test]
    fn test_derive_peer_endpoint_id_deterministic() {
        let secret = b"test-formation-secret".to_vec();
        let store_secret = secret.clone();

        // We can't easily build a real NetworkedIrohBlobStore in a sync test,
        // so test the HKDF derivation directly.
        let derive = |secret: &[u8], hostname: &str| -> EndpointId {
            let hk = Hkdf::<Sha256>::new(None, secret);
            let mut okm = [0u8; 32];
            let context = format!("iroh:{}", hostname);
            hk.expand(context.as_bytes(), &mut okm).unwrap();
            SecretKey::from_bytes(&okm).public()
        };

        let id1 = derive(&secret, "peat-mesh-0");
        let id2 = derive(&store_secret, "peat-mesh-0");
        assert_eq!(
            id1, id2,
            "Same secret + hostname must produce same EndpointId"
        );
    }

    /// Different hostnames must produce different EndpointIds.
    #[test]
    fn test_derive_peer_endpoint_id_different_hosts() {
        let secret = b"test-formation-secret".to_vec();

        let derive = |hostname: &str| -> EndpointId {
            let hk = Hkdf::<Sha256>::new(None, &secret);
            let mut okm = [0u8; 32];
            let context = format!("iroh:{}", hostname);
            hk.expand(context.as_bytes(), &mut okm).unwrap();
            SecretKey::from_bytes(&okm).public()
        };

        let id_a = derive("peat-mesh-0");
        let id_b = derive("peat-mesh-1");
        assert_ne!(
            id_a, id_b,
            "Different hostnames must produce different EndpointIds"
        );
    }

    /// Different secrets must produce different EndpointIds.
    #[test]
    fn test_derive_peer_endpoint_id_different_secrets() {
        let derive = |secret: &[u8], hostname: &str| -> EndpointId {
            let hk = Hkdf::<Sha256>::new(None, secret);
            let mut okm = [0u8; 32];
            let context = format!("iroh:{}", hostname);
            hk.expand(context.as_bytes(), &mut okm).unwrap();
            SecretKey::from_bytes(&okm).public()
        };

        let id_a = derive(b"secret-one", "peat-mesh-0");
        let id_b = derive(b"secret-two", "peat-mesh-0");
        assert_ne!(
            id_a, id_b,
            "Different secrets must produce different EndpointIds"
        );
    }
}
