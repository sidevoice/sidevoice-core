//! The machine's "send crash reports" setting, for the app installed on the same machine (`rust/crash/settings.rs`).
//!
//! `GET` says it and `PUT {"send": bool}` sets it. Both are under `/api/local`, so they exist only on the same user's local
//! channel (a Unix socket, a named pipe): on TCP, over the network, for a paired remote device and for a browser (a request
//! with an `Origin`) they are not found, whatever token comes with them, and the connector link (`/api/connectors/v3`) has no
//! method for them. The change goes one way, app to core: the command line's `crash-reports on|off` writes the same file and
//! tells nobody. Off erases the machine id and every pending record, as the command does.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tracing::{info, warn};

use super::refusal::{refuse, Handled};
use super::AppState;
use crate::error::CoreError;

pub(super) fn local_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/local/crash-reports", get(read).put(write))
}

fn unavailable(error: &std::io::Error, headers: &HeaderMap) -> super::refusal::Refusal {
    warn!(
        error.type = CoreError::CrashReportsUnavailable.code(),
        kind = ?error.kind(),
        "the crash report setting could not be read or changed"
    );
    refuse("start.failed", StatusCode::INTERNAL_SERVER_ERROR, headers)
}

async fn read(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Handled {
    match crate::crash::setting(state.dir.path()) {
        Ok(send) => Ok(Json(json!({"send": send})).into_response()),
        Err(error) => Err(unavailable(&error, &headers)),
    }
}

/// The body is a JSON object with a boolean `send`; other fields are ignored.
async fn write(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Handled {
    let Some(send) = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|body| body.get("send")?.as_bool())
    else {
        return Err(refuse(
            "settings.crash_reports_invalid",
            StatusCode::BAD_REQUEST,
            &headers,
        ));
    };
    crate::crash::set_enabled(state.dir.path(), send)
        .map_err(|error| unavailable(&error, &headers))?;
    info!(
        send,
        result = "changed",
        "the crash report setting was changed by the app on this machine"
    );
    Ok(Json(json!({"send": send})).into_response())
}
