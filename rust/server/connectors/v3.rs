//! Symmetric JSON-RPC 2.0 on the local Unix listener.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::server::AppState;

use super::{attach, field};

mod connection;
mod dispatch;
mod frame;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTOR_ID: usize = 200;
const MAX_TOKEN: usize = 512;

pub(super) async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| run(state, socket))
        .into_response()
}

async fn run(state: Arc<AppState>, mut socket: WebSocket) {
    let Some((hello_id, cid)) = handshake(&state, &mut socket).await else {
        return;
    };
    let attached = attach(&state.room, cid);
    connection::Connection::open(state, socket, attached, hello_id)
        .await
        .serve()
        .await;
}

/// Reads the connector's `connector.hello` and checks its credential, refusing
/// the socket otherwise. Returns the hello's id and the connector id.
async fn handshake(state: &AppState, socket: &mut WebSocket) -> Option<(Value, String)> {
    let first = tokio::time::timeout(HELLO_TIMEOUT, socket.recv()).await;
    let hello = match first.ok().flatten() {
        Some(Ok(Message::Text(raw))) => frame::decode(&raw).ok(),
        _ => None,
    };
    let Some(hello) =
        hello.filter(|hello| hello["method"] == "connector.hello" && hello.get("id").is_some())
    else {
        refuse(socket, None).await;
        return None;
    };
    let params = hello.get("params").cloned().unwrap_or(json!({}));
    let id = hello["id"].clone();
    let cid = field(&params, "connector_id");
    let token = field(&params, "token");
    if params["protocol"] != 3 || !bounded(cid, MAX_CONNECTOR_ID) || !bounded(token, MAX_TOKEN) {
        refuse(
            socket,
            Some(frame::error(id, -32002, "Protocol 3 hello is required")),
        )
        .await;
        return None;
    }
    if !state.room.authenticate_connector(cid, token, &params) {
        refuse(
            socket,
            Some(frame::error(id, -32001, "Connector credential refused")),
        )
        .await;
        return None;
    }
    Some((id, cid.to_owned()))
}

fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max
}

async fn refuse(socket: &mut WebSocket, reply: Option<Value>) {
    if let Some(message) = reply.and_then(frame::text) {
        let _ = socket.send(message).await;
    }
    let _ = socket.send(frame::close(frame::POLICY_VIOLATION)).await;
}

#[cfg(test)]
mod tests;
