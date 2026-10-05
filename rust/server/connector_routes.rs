//! Device routes about the pinned connector: its listing, and the host agents it
//! manages, proxied to it as requests.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use super::refusal::{refuse, require_origin, Handled, Refusal};
use super::request::query;
use super::AppState;

mod answer;

const AGENT_TIMEOUT: Duration = Duration::from_secs(20);
const AGENT_ACTIONS: [&str; 3] = ["connect", "disconnect", "dismiss"];

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/connectors", get(connector_listing))
        .route("/api/host/agents", get(host_agents))
        .route(
            "/api/host/agents/{agent_id}/{action}",
            post(host_agent_action),
        )
}

async fn connector_listing(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Handled {
    require_origin(&headers)?;
    Ok(Json(
        json!({"connectors":state.room.paired_connectors(),"bindings":state.room.binding_views()}),
    )
    .into_response())
}

async fn host_agents(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Handled {
    require_origin(&headers)?;
    let rescan = query(&uri, "rescan").unwrap_or_default();
    let force = match rescan.to_ascii_lowercase().as_str() {
        "" | "0" | "false" => false,
        "1" | "true" => true,
        _ => return Err(keyed(StatusCode::BAD_REQUEST, "invalid-rescan").into()),
    };
    let watch = query(&uri, "watch");
    if watch.as_ref().is_some_and(|s| !agent_id_valid(s)) {
        return Err(keyed(StatusCode::BAD_REQUEST, "invalid-agent-id").into());
    }
    let Some(peer) = state.room.connector_peer() else {
        return Err(no_connector());
    };
    let mut params = json!({"rescan":force});
    if let Some(watch) = watch {
        params["watch"] = json!(watch)
    }
    Ok(answer::host_agent_response(
        peer.request("agents.list", params, AGENT_TIMEOUT).await,
    ))
}

async fn host_agent_action(
    State(state): State<Arc<AppState>>,
    Path((agent_id, action)): Path<(String, String)>,
    headers: HeaderMap,
) -> Handled {
    require_origin(&headers)?;
    if !agent_id_valid(&agent_id) {
        return Err(keyed(StatusCode::BAD_REQUEST, "invalid-agent-id").into());
    }
    if !AGENT_ACTIONS.contains(&action.as_str()) {
        return Err(refuse("request.not_found", StatusCode::NOT_FOUND, &headers));
    }
    let Some(peer) = state.room.connector_peer() else {
        return Err(no_connector());
    };
    Ok(answer::host_agent_response(
        peer.request(
            &format!("agents.{action}"),
            json!({"id":agent_id}),
            AGENT_TIMEOUT,
        )
        .await,
    ))
}

fn agent_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && (id.as_bytes()[0].is_ascii_lowercase() || id.as_bytes()[0].is_ascii_digit())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// A refusal the device client localizes from its bare key.
fn keyed(status: StatusCode, key: &str) -> Response {
    (status, Json(json!({ "key": key }))).into_response()
}

fn no_connector() -> Refusal {
    keyed(StatusCode::SERVICE_UNAVAILABLE, "no-connector").into()
}

#[cfg(test)]
mod tests;
