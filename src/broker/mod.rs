//! HTTP/WS service broker for the mesh (ADR-049 Phase 6).
//!
//! Feature-gated under `"broker"`. Provides an Axum-based HTTP REST API
//! and WebSocket endpoint so external applications can observe and interact
//! with the mesh at runtime.

pub mod error;
#[cfg(feature = "lite-bridge")]
pub mod ota_routes;
pub mod routes;
pub mod server;
pub mod state;
pub mod ws;

#[cfg(feature = "automerge-backend")]
pub mod composite_state;
#[cfg(feature = "automerge-backend")]
pub mod store_adapter;

pub use error::BrokerError;
pub use server::{Broker, BrokerConfig};
pub use state::{MeshBrokerState, MeshEvent, MeshNodeInfo, PeerSummary, TopologySummary};

#[cfg(feature = "automerge-backend")]
pub use composite_state::CompositeBrokerState;
#[cfg(feature = "automerge-backend")]
pub use store_adapter::StoreBrokerAdapter;

#[cfg(feature = "lite-bridge")]
pub use ota_routes::OtaAppState;
