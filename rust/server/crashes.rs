//! The crash records of this machine, for paired apps only (`rust/crash.rs`): list them, and acknowledge one once the
//! app has handled it.
//!
//! Both routes are on the device surface: a paired device's bearer token is required, on TCP and on the socket alike.
//! They are not under a local-only prefix, so the socket does not open them without a token, and the connector link
//! (`/api/connectors/*`, the JSON-RPC methods of `connectors/v3`) has no method for them. The machine never sends a
//! record: the app reads it here and decides.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde_json::json;
use tracing::{info, warn};

use super::refusal::{refuse, require_origin, Handled};
use super::{AppState, AuthenticatedDevice};
use crate::error::CoreError;
use crate::log_safe::ClientId;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/device/crashes", get(list))
        .route("/api/device/crashes/{crash_id}", delete(acknowledge))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    device: Option<Extension<AuthenticatedDevice>>,
) -> Handled {
    require_origin(&headers)?;
    if device.is_none() {
        return Err(unpaired(&headers));
    }
    match state.crashes.list() {
        Ok(records) => Ok(Json(json!({"crashes": records})).into_response()),
        Err(error) => {
            warn!(
                error.type = CoreError::CrashRecordsUnavailable.code(),
                kind = ?error.kind(),
                "the crash records could not be read"
            );
            Err(refuse(
                "start.failed",
                StatusCode::INTERNAL_SERVER_ERROR,
                &headers,
            ))
        }
    }
}

/// Logs the device and the record's signature, never its id: the id may become a Sentry `event_id` before it is sent.
async fn acknowledge(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    device: Option<Extension<AuthenticatedDevice>>,
) -> Handled {
    require_origin(&headers)?;
    let Some(Extension(device)) = device else {
        return Err(unpaired(&headers));
    };
    match state.crashes.acknowledge(&id) {
        Ok(Some(signature)) => {
            info!(
                device = %ClientId(&device.0),
                signature = %signature,
                result = "acknowledged",
                "a crash record was handled by an app and deleted"
            );
            Ok(Json(json!({"ok": true})).into_response())
        }
        Ok(None) => Err(refuse("crash.not_found", StatusCode::NOT_FOUND, &headers)),
        Err(error) => {
            warn!(
                error.type = CoreError::CrashRecordsUnavailable.code(),
                kind = ?error.kind(),
                "a crash record could not be deleted"
            );
            Err(refuse(
                "start.failed",
                StatusCode::INTERNAL_SERVER_ERROR,
                &headers,
            ))
        }
    }
}

fn unpaired(headers: &HeaderMap) -> super::refusal::Refusal {
    refuse("device.unpaired", StatusCode::UNAUTHORIZED, headers)
}
