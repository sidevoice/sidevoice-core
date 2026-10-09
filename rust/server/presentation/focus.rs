//! Changing which conversation a call talks to: select, leave and close.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;

use crate::server::refusal::{require_origin, room_refusal, Handled};
use crate::server::request::room_payload;
use crate::server::AppState;

pub(super) async fn select(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let data = room_payload(&body, &headers)?;
    let sid = data["session_id"].as_str().unwrap_or("");
    let selected = state
        .room
        .select(sid, data["thread_id"].as_str().unwrap_or(""))
        .map_err(|error| room_refusal(error, &headers))?;
    Ok(Json(selected).into_response())
}

pub(super) async fn leave(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let data = room_payload(&body, &headers)?;
    let sid = data["session_id"].as_str().unwrap_or("");
    let left = state
        .room
        .deselect(sid, data["binding_id"].as_str().unwrap_or(""))
        .map_err(|error| room_refusal(error, &headers))?;
    Ok(Json(left).into_response())
}

pub(super) async fn close(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let data = room_payload(&body, &headers)?;
    let (result, notify) = state
        .room
        .close_channel(data["thread_id"].as_str().unwrap_or(""))
        .map_err(|error| room_refusal(error, &headers))?;
    if let Some((peer, params)) = notify {
        let _ = peer.send("binding.close", params).await;
    }
    Ok(Json(result).into_response())
}
