//! What the node says about itself, and the local request to leave.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::runtime::API;

use super::refusal::{refuse, Handled};
use super::AppState;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/rendezvous", get(rendezvous))
}

pub(super) fn local_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/local/health", get(health))
        .route("/api/local/shutdown", post(shutdown))
}

async fn rendezvous(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"kind": "node", "fingerprint": state.identity.fingerprint, "api": API}))
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(
        json!({"launch_id": state.launch_id, "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"), "api": API, "fingerprint": state.identity.fingerprint,
        "public_key": state.identity.public_key, "host": state.host, "calls": state.open_calls()}),
    )
}

/// `POST /api/local/shutdown`: leave as on `SIGTERM` (finish the requests in flight, remove the ready file, exit 0).
/// The in-band request to leave, the only one a console-less core on Windows can get (decision "Graceful stop",
/// 2026-10-22); the same on every OS. Local channel only, like every `/api/local` route.
///
/// Body optional, JSON: `{"launch_id": "..."}` names the launch the caller means (from the ready file); another
/// launch refuses with 409 `runtime.launch_mismatch` and stays up. Unknown fields, an empty body or one that is not
/// an object are read as naming no launch. Answers 202 `{"launch_id", "pid"}` before the process stops. A core that
/// predates this route answers 404 `request.not_found`: the caller falls back to the OS stop.
async fn shutdown(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Handled {
    let named = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|body| body.get("launch_id")?.as_str().map(str::to_owned));
    if named.is_some_and(|launch| launch != state.launch_id) {
        return Err(refuse(
            "runtime.launch_mismatch",
            StatusCode::CONFLICT,
            &headers,
        ));
    }
    state.shutdown.notify_one();
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"launch_id": state.launch_id, "pid": std::process::id()})),
    )
        .into_response())
}
