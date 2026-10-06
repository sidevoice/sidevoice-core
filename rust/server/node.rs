//! What the node says about itself, and the device's request to pair it with a room.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::runtime::API;

use super::refusal::{refuse, Handled};
use super::trust::origin_allowed;
use super::AppState;

const PAIR_TIMEOUT: Duration = Duration::from_secs(25);

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/rendezvous", get(rendezvous))
        .route("/api/rendezvous/pair", post(rendezvous_pair))
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

#[derive(Deserialize)]
struct PairRoom {
    room: String,
    code: String,
}

/// A pairing the connector refused: its own reason when it gave one (passed through as the Python core did), else
/// the generic one.
pub(super) fn pair_refused(answer: &Value, headers: &HeaderMap) -> axum::response::Response {
    match answer
        .get("detail")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
    {
        Some(detail) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": detail.chars().take(1000).collect::<String>()})),
        )
            .into_response(),
        None => super::refusal::failure("relay.pair_failed", StatusCode::BAD_REQUEST, headers),
    }
}

/// Asks the pinned connector to pair this node with a room.
async fn rendezvous_pair(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    // Unlike other routes, a browser origin is required here, not merely allowed.
    if headers.get(header::ORIGIN).is_none() || !origin_allowed(&headers) {
        return Err(refuse(
            "request.origin_invalid",
            StatusCode::FORBIDDEN,
            &headers,
        ));
    }
    let pair_invalid = || {
        refuse(
            "relay.pair_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        )
    };
    let input = serde_json::from_slice::<PairRoom>(&body).map_err(|_| pair_invalid())?;
    let room = input.room.trim();
    let code = input.code.trim();
    if room.is_empty() || room.len() > 2048 || code.is_empty() || code.len() > 64 {
        return Err(pair_invalid());
    }
    let Some(peer) = state.room.connector_peer() else {
        return Err(refuse(
            "relay.connector_unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers,
        ));
    };
    // The route's own deadline around the peer's tells a timeout from a connector that could not answer at all.
    let answer = tokio::time::timeout(
        PAIR_TIMEOUT,
        peer.request(
            "pair.request",
            json!({"room":room,"code":code}),
            PAIR_TIMEOUT + Duration::from_secs(1),
        ),
    )
    .await
    .map_err(|_| refuse("relay.pair_timeout", StatusCode::GATEWAY_TIMEOUT, &headers))?
    .map_err(|_| {
        refuse(
            "relay.connector_unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers,
        )
    })?;
    if answer.get("ok") != Some(&Value::Bool(true)) {
        return Err(pair_refused(&answer, &headers).into());
    }
    state.rendezvous.poke();
    Ok(Json(
        json!({"ok":true,"room":answer.get("origin"),"connector_id":answer.get("connector_id")}),
    )
    .into_response())
}
