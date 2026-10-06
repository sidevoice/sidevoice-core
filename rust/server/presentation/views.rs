//! Read-only room views: the snapshot, admission, participants and history.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, Uri};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use super::replay::mark_replayable;
use crate::server::refusal::{require_origin, Handled};
use crate::server::request::query;
use crate::server::{AppState, AuthenticatedDevice};

pub(super) async fn state(State(state): State<Arc<AppState>>, uri: Uri) -> Json<Value> {
    Json(state.room.snapshot(query(&uri, "session_id").as_deref()))
}

pub(super) async fn admission(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<Value> {
    // The whole header goes to the room; rendering keeps only its primary subtag.
    Json(
        state.room.admission(
            headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("en"),
        ),
    )
}

pub(super) async fn participants(State(state): State<Arc<AppState>>, uri: Uri) -> Json<Value> {
    Json(json!({"participants":state.room.participants(query(&uri,"session_id").as_deref())}))
}

pub(super) async fn history(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    uri: Uri,
    headers: HeaderMap,
) -> Handled {
    require_origin(&headers)?;
    let mut history = state.room.history(query(&uri, "thread_id").as_deref());
    if let Some(sid) =
        query(&uri, "session_id").filter(|sid| state.room.owns_session(sid, &device.0))
    {
        mark_replayable(&state, &sid, &mut history);
    }
    Ok(Json(history).into_response())
}
