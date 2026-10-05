//! The connector's requests to the node, by method.

use std::sync::Arc;

use serde_json::Value;

use crate::server::connectors::{echo_ids, field, register_binding, Link, Notification};
use crate::server::AppState;

const MAX_SPEECH_ID: usize = 200;

/// A JSON-RPC error: its code and message.
pub(super) type RpcError = (i64, &'static str);

pub(super) async fn dispatch(
    state: Arc<AppState>,
    link: Link,
    method: String,
    params: Value,
) -> Result<Value, RpcError> {
    let room = &state.room;
    if !link.current(room) {
        return Err((-32001, "room.connector_disconnected"));
    }
    if let Some(notification) = Notification::parse(&method) {
        notification.apply(room, &link.cid, &params);
        return Ok(Value::Null);
    }
    match method.as_str() {
        "binding.register" => Ok(register_binding(room, &link.cid, &params)),
        "speech.publish" => {
            if ["event_id", "utterance_id"].iter().any(|key| {
                let s = field(&params, key);
                s.is_empty() || s.len() > MAX_SPEECH_ID
            }) {
                return Err((-32602, "speech.publish requires bounded identifiers"));
            }
            let result = room.connector_speech(&link.cid, &params, true);
            Ok(echo_ids(result, &params, &["event_id", "utterance_id"]))
        }
        "input.pull" => room
            .pull_input(&link.cid, &params)
            .map_err(|error| (-(error.status as i64), error.key)),
        "device.pairing_code" => Ok(state.issue_code()),
        _ => Err((-32601, "Method not found")),
    }
}
