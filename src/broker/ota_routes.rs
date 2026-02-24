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
            Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::to_value(response).unwrap()),
            ))
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
        Some(info) => Ok(Json(serde_json::to_value(info).unwrap())),
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
}
