//! A paired device's call socket: authentication by subprotocol, then one call.

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{header, HeaderMap};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde_json::Value;

use crate::messages::{render, LocalizedMessage};

use super::refusal::{require_origin, Handled};
use super::request::accept_language;
use super::AppState;

mod admission;
mod registration;
mod session;

pub(super) use registration::CallRegistry;

const SUBPROTOCOL: &str = "sidevoice";
const TOKEN_PREFIX: &str = "sidevoice.token.";
/// The close code a device reads as "pair again".
const UNPAIRED: u16 = 4401;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/presentation/ws", get(call_socket))
}

async fn call_socket(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Handled {
    require_origin(&headers)?;
    let protocols: Vec<_> = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .collect();
    let device = protocols
        .iter()
        .find_map(|value| value.strip_prefix(TOKEN_PREFIX))
        .and_then(|token| state.authenticate_token(token));
    let close_reason = render(
        &LocalizedMessage::new("device.unpaired"),
        accept_language(&headers),
    );
    let ws = if protocols.contains(&SUBPROTOCOL) {
        ws.protocols([SUBPROTOCOL])
    } else {
        ws
    };
    Ok(ws
        .on_upgrade(move |socket| session::run(state, device, socket, close_reason))
        .into_response())
}

fn text(value: &Value) -> Message {
    Message::Text(value.to_string().into())
}

fn close(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

#[cfg(test)]
mod tests;
