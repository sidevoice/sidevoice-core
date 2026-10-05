//! Device trust routes: node identity proof, pairing, listing and revocation.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{body::Bytes, Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::control::devices::valid_nonce;

use super::refusal::{refuse, require_origin, Handled, Refusal};
use super::request::query;
use super::{AppState, AuthenticatedDevice};

const MAX_FIELD: usize = 200;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/device/identity", get(identity))
        .route("/api/device/pair", post(pair))
        .route("/api/device/devices", get(devices))
        .route("/api/device/devices/{device_id}", delete(revoke))
}

pub(super) fn local_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/device/local/pair", post(pair_local))
        .route("/api/device/local", delete(unpair_local))
}

async fn identity(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Handled {
    require_origin(&headers)?;
    let nonce = query(&uri, "nonce").unwrap_or_default();
    if !valid_nonce(&nonce) {
        return Err(refuse(
            "device.nonce_invalid",
            StatusCode::BAD_REQUEST,
            &headers,
        ));
    }
    Ok(Json(
        json!({"fingerprint": state.identity.fingerprint, "public_key": state.identity.public_key,
        "signature": state.identity.sign(&nonce)}),
    )
    .into_response())
}

#[derive(Deserialize)]
struct Pair {
    secret: String,
    name: Option<String>,
}

async fn pair(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Handled {
    require_origin(&headers)?;
    let secret_invalid = |status| refuse("device.secret_invalid", status, &headers);
    let Ok(payload) = serde_json::from_slice::<Pair>(&body) else {
        return Err(secret_invalid(StatusCode::BAD_REQUEST));
    };
    if payload.secret.is_empty()
        || payload.secret.len() > MAX_FIELD
        || name_too_long(payload.name.as_deref())
    {
        return Err(secret_invalid(StatusCode::BAD_REQUEST));
    }
    let result = state
        .registry
        .lock()
        .expect("registry lock")
        .redeem(&payload.secret, payload.name.as_deref());
    match result {
        Ok(Some((device_id, token))) => Ok(Json(
            json!({"device_id": device_id, "token": token, "node": state.node()}),
        )
        .into_response()),
        Ok(None) => Err(secret_invalid(StatusCode::FORBIDDEN)),
        Err(_) => Err(start_failed(&headers)),
    }
}

async fn devices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    device: Option<Extension<AuthenticatedDevice>>,
) -> Handled {
    require_origin(&headers)?;
    let Some(Extension(AuthenticatedDevice(current))) = device else {
        return Err(unpaired(&headers));
    };
    let listing = state
        .registry
        .lock()
        .expect("registry lock")
        .listing(&current);
    Ok(Json(listing).into_response())
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    device: Option<Extension<AuthenticatedDevice>>,
) -> Handled {
    require_origin(&headers)?;
    if device.is_none() {
        return Err(unpaired(&headers));
    }
    let revoked = state.registry.lock().expect("registry lock").revoke(&id);
    match revoked {
        Ok(true) => {
            state.close_calls(&id);
            Ok(Json(json!({"ok": true})).into_response())
        }
        Ok(false) => Err(refuse("device.not_found", StatusCode::NOT_FOUND, &headers)),
        Err(_) => Err(start_failed(&headers)),
    }
}

#[derive(Deserialize, Default)]
struct LocalPair {
    name: Option<String>,
}

async fn pair_local(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    let secret_invalid = || refuse("device.secret_invalid", StatusCode::BAD_REQUEST, &headers);
    let payload = if body.is_empty() {
        LocalPair::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| secret_invalid())?
    };
    if name_too_long(payload.name.as_deref()) {
        return Err(secret_invalid());
    }
    let result = state
        .registry
        .lock()
        .expect("registry lock")
        .pair_local(payload.name.as_deref());
    let (device_id, token, replaced) = result.map_err(|_| start_failed(&headers))?;
    for id in replaced {
        state.close_calls(&id);
    }
    Ok(Json(json!({"device_id": device_id, "token": token, "node": state.node()})).into_response())
}

async fn unpair_local(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Handled {
    let result = state.registry.lock().expect("registry lock").revoke_local();
    let revoked = result.map_err(|_| start_failed(&headers))?;
    for id in &revoked {
        state.close_calls(id);
    }
    Ok(Json(json!({"ok": true, "revoked": !revoked.is_empty()})).into_response())
}

fn name_too_long(name: Option<&str>) -> bool {
    name.is_some_and(|name| name.len() > MAX_FIELD)
}

fn unpaired(headers: &HeaderMap) -> Refusal {
    refuse("device.unpaired", StatusCode::UNAUTHORIZED, headers)
}

fn start_failed(headers: &HeaderMap) -> Refusal {
    refuse("start.failed", StatusCode::INTERNAL_SERVER_ERROR, headers)
}
