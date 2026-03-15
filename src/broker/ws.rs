//! WebSocket upgrade handler for streaming mesh events.

use super::state::MeshBrokerState;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use std::sync::Arc;

/// GET /api/v1/ws — upgrades to WebSocket and streams `MeshEvent`s as JSON.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<dyn MeshBrokerState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<dyn MeshBrokerState>) {
    let mut rx = state.subscribe_events();

    loop {
        tokio::select! {
            // Forward mesh events to the client as JSON text frames.
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        let json = match serde_json::to_string(&event) {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::warn!("failed to serialize mesh event: {}", e);
                                continue;
                            }
                        };
                        if socket.send(Message::Text(json)).await.is_err() {
                            // Client disconnected.
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("ws client lagged, skipped {} events", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Sender dropped — shut down gracefully.
                        break;
                    }
                }
            }
            // If the client sends a close or the connection drops, exit.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // ignore other client messages
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::broker::state::MeshEvent;

    #[test]
    fn ws_handler_exists() {
        // Compile-time check -- functional WS tests live in server.rs
        // (test_ws_streams_events, test_ws_multiple_event_types,
        // test_ws_sender_drop_closes_stream).
        let _ = super::ws_handler as fn(_, _) -> _;
    }

    #[test]
    fn test_mesh_event_serializes_to_json_text() {
        // Verify the JSON serialization that handle_socket uses for each event type.
        let event = MeshEvent::PeerConnected {
            peer_id: "peer-1".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"PeerConnected\""));
        assert!(json.contains("\"peer_id\":\"peer-1\""));
    }

    #[test]
    fn test_peer_disconnected_event_serialization() {
        let event = MeshEvent::PeerDisconnected {
            peer_id: "peer-2".into(),
            reason: "timed out".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "PeerDisconnected");
        assert_eq!(parsed["peer_id"], "peer-2");
        assert_eq!(parsed["reason"], "timed out");
    }

    #[test]
    fn test_topology_changed_event_serialization() {
        let event = MeshEvent::TopologyChanged {
            new_role: "follower".into(),
            peer_count: 7,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "TopologyChanged");
        assert_eq!(parsed["new_role"], "follower");
        assert_eq!(parsed["peer_count"], 7);
    }

    #[test]
    fn test_sync_event_serialization() {
        let event = MeshEvent::SyncEvent {
            collection: "beacons".into(),
            doc_id: "doc-42".into(),
            action: "merge".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "SyncEvent");
        assert_eq!(parsed["collection"], "beacons");
        assert_eq!(parsed["doc_id"], "doc-42");
        assert_eq!(parsed["action"], "merge");
    }

    #[test]
    fn test_all_event_types_are_tagged_union() {
        // The serde(tag = "type") attribute means every variant has a "type" field.
        let events: Vec<MeshEvent> = vec![
            MeshEvent::PeerConnected {
                peer_id: "a".into(),
            },
            MeshEvent::PeerDisconnected {
                peer_id: "b".into(),
                reason: "x".into(),
            },
            MeshEvent::TopologyChanged {
                new_role: "r".into(),
                peer_count: 0,
            },
            MeshEvent::SyncEvent {
                collection: "c".into(),
                doc_id: "d".into(),
                action: "a".into(),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(
                parsed.get("type").is_some(),
                "Event should have a 'type' field: {}",
                json
            );
        }
    }
}
