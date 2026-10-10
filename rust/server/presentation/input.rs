//! A device's writes into the room: text, input cancellation, client errors and speech.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use uuid::Uuid;

use crate::server::refusal::{refuse, require_origin, room_refusal, Handled};
use crate::server::request::room_payload;
use crate::server::{AppState, AuthenticatedDevice};

pub(super) async fn text(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let data = room_payload(&body, &headers)?;
    let text = data["text"].as_str().unwrap_or("");
    let mid = data["message_id"].as_str().unwrap_or("");
    if Uuid::parse_str(mid).is_err() {
        return Err(refuse(
            "room.request_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        ));
    }
    let sent = state
        .room
        .send_text(
            text,
            data["session_id"].as_str().unwrap_or(""),
            data["thread_id"].as_str().unwrap_or(""),
            data["binding_id"].as_str().unwrap_or(""),
            mid,
        )
        .map_err(|error| room_refusal(error, &headers))?;
    Ok(Json(sent).into_response())
}

pub(super) async fn cancel_input(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let data = room_payload(&body, &headers)?;
    let sid = data["session_id"].as_str().unwrap_or("");
    let turn_id = data["turn_id"].as_str().unwrap_or("");
    if !state.room.owns_session(sid, &device.0) {
        return Err(refuse(
            "room.browser_absent",
            StatusCode::CONFLICT,
            &headers,
        ));
    }
    let result = state
        .room
        .cancel_input(sid, turn_id)
        .map_err(|error| room_refusal(error, &headers))?;
    Ok(Json(result).into_response())
}

pub(super) async fn client_error(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let data = room_payload(&body, &headers)?;
    Ok(Json(state.room.report_client_error(&data)).into_response())
}
