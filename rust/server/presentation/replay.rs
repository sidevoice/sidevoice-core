//! A reply the person asks to hear again: sent to their call once more, for its voice module to speak.

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

pub(super) async fn replay(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let data = room_payload(&body, &headers)?;
    let sid = data["session_id"].as_str().unwrap_or("");
    let history_id = data["history_id"].as_str().unwrap_or("");
    if !state.room.owns_session(sid, &device.0) {
        return Err(refuse(
            "room.browser_absent",
            StatusCode::CONFLICT,
            &headers,
        ));
    }
    if history_id.is_empty() {
        return Err(refuse(
            "room.replay_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        ));
    }
    let uid = format!("{sid}:replay:{}", Uuid::new_v4());
    let replayed = state
        .room
        .replay_one(sid, history_id, &uid)
        .map_err(|error| room_refusal(error, &headers))?;
    Ok(Json(replayed).into_response())
}
