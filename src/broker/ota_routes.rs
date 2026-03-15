//! OTA firmware update HTTP API routes.
//!
//! Provides endpoints for uploading firmware and monitoring OTA progress:
//! - `POST /api/v1/ota/:peer_id` — upload firmware for delivery to a Lite peer
//! - `GET /api/v1/ota/:peer_id/status` — query transfer progress

use crate::transport::lite_ota::{FirmwareImage, OtaSender};
use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    Json,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// Shared OTA state accessible from route handlers
pub struct OtaAppState {
    pub sender: Arc<OtaSender>,
}

/// Response for a successful OTA upload
#[derive(Debug, Serialize)]
pub struct OtaUploadResponse {
    pub session_id: u16,
    pub peer_id: String,
    pub firmware_size: usize,
    pub total_chunks: u16,
    pub version: String,
}

/// POST /api/v1/ota/:peer_id
///
/// Accepts a multipart form with:
/// - `firmware`: the firmware binary file
/// - `version`: version string (e.g., "0.2.0")
pub async fn upload_firmware(
    State(state): State<Arc<OtaAppState>>,
    Path(peer_id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let mut firmware_data: Option<Vec<u8>> = None;
    let mut version: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "firmware" => {
                firmware_data = field.bytes().await.ok().map(|b| b.to_vec());
            }
            "version" => {
                version = field.text().await.ok();
            }
            _ => {}
        }
    }

    let firmware_data = firmware_data.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing 'firmware' field"})),
        )
    })?;

    let version = version.unwrap_or_else(|| "0.0.0".to_string());

    if firmware_data.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "firmware file is empty"})),
        ));
    }

    // Max 3MB firmware (matches OTA partition size)
    if firmware_data.len() > 0x300000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "firmware too large (max 3MB)"})),
        ));
    }

    let firmware = FirmwareImage::new(firmware_data.clone(), &version);
    let total_chunks = firmware.total_chunks;
    let peer_node_id = crate::transport::NodeId::new(peer_id.clone());

    match state.sender.offer_to_peer(&peer_node_id, firmware).await {
        Ok(session_id) => {
            let response = OtaUploadResponse {
                session_id,
                peer_id,
                firmware_size: firmware_data.len(),
                total_chunks,
                version,
            };
            let json = serde_json::to_value(response).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("failed to serialize response: {}", e)})),
                )
            })?;
            Ok((StatusCode::ACCEPTED, Json(json)))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to start OTA: {}", e)})),
        )),
    }
}

/// GET /api/v1/ota/:peer_id/status
pub async fn ota_status(
    State(state): State<Arc<OtaAppState>>,
    Path(peer_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.sender.get_status(&peer_id).await {
        Some(info) => {
            let json = serde_json::to_value(info).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("failed to serialize status: {}", e)})),
                )
            })?;
            Ok(Json(json))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("no OTA session for peer: {}", peer_id)})),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ota_upload_response_serialization() {
        let resp = OtaUploadResponse {
            session_id: 1,
            peer_id: "lite-4D355443".to_string(),
            firmware_size: 1024,
            total_chunks: 3,
            version: "0.2.0".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"session_id\":1"));
        assert!(json.contains("lite-4D355443"));
        assert!(json.contains("\"firmware_size\":1024"));
    }

    #[test]
    fn test_ota_upload_response_all_fields_present() {
        let resp = OtaUploadResponse {
            session_id: 42,
            peer_id: "peer-xyz".to_string(),
            firmware_size: 3_145_728,
            total_chunks: 768,
            version: "1.0.0-rc1".to_string(),
        };
        let json: serde_json::Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["session_id"], 42);
        assert_eq!(json["peer_id"], "peer-xyz");
        assert_eq!(json["firmware_size"], 3_145_728);
        assert_eq!(json["total_chunks"], 768);
        assert_eq!(json["version"], "1.0.0-rc1");
    }

    #[test]
    fn test_ota_upload_response_debug_impl() {
        let resp = OtaUploadResponse {
            session_id: 1,
            peer_id: "p".to_string(),
            firmware_size: 0,
            total_chunks: 0,
            version: "0".to_string(),
        };
        let debug = format!("{:?}", resp);
        assert!(debug.contains("OtaUploadResponse"));
        assert!(debug.contains("session_id"));
    }

    #[test]
    fn test_ota_upload_response_zero_size() {
        // Edge case: zero-sized firmware should still serialize
        let resp = OtaUploadResponse {
            session_id: 0,
            peer_id: "".to_string(),
            firmware_size: 0,
            total_chunks: 0,
            version: "".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["firmware_size"], 0);
        assert_eq!(parsed["total_chunks"], 0);
    }

    #[test]
    fn test_ota_upload_response_max_session_id() {
        let resp = OtaUploadResponse {
            session_id: u16::MAX,
            peer_id: "max-session".to_string(),
            firmware_size: 1,
            total_chunks: 1,
            version: "0.0.1".to_string(),
        };
        let json: serde_json::Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["session_id"], u16::MAX as u64);
    }
}
