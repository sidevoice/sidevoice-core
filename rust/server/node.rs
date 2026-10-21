//! What the node says about itself.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::runtime::API;

use super::AppState;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/rendezvous", get(rendezvous))
}

pub(super) fn local_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/local/health", get(health))
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
