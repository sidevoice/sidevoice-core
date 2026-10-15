//! Read-only room views: the snapshot, admission, participants and history.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::{header, HeaderMap, Uri};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::server::refusal::{refuse, require_origin, Handled};
use crate::server::request::query;
use crate::server::AppState;

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
    uri: Uri,
    headers: HeaderMap,
) -> Handled {
    require_origin(&headers)?;
    let resolve_ids: Vec<String> = uri
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .filter(|(name, _)| name == "resolve_id")
                .map(|(_, id)| id.into_owned())
                .collect()
        })
        .unwrap_or_default();
    let thread = query(&uri, "thread_id");
    if resolve_ids.len() > 64
        || resolve_ids.iter().any(|id| id.is_empty() || id.len() > 256)
        || (!resolve_ids.is_empty() && thread.as_deref().is_none_or(str::is_empty))
    {
        return Err(refuse(
            "room.request_invalid",
            StatusCode::BAD_REQUEST,
            &headers,
        ));
    }
    let history = state
        .room
        .history_resolving(thread.as_deref(), &resolve_ids);
    Ok(Json(history).into_response())
}
