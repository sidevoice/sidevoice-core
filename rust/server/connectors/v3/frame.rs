//! JSON-RPC 2.0 frames: decoding with validation, and encoding within the size bound.

use axum::extract::ws::{CloseFrame, Message};
use serde_json::{json, Value};

const MAX_FRAME: usize = 1024 * 1024;
/// The largest integer a JavaScript peer can represent exactly.
const MAX_SAFE_ID: i64 = 9_007_199_254_740_991;
const MAX_METHOD: usize = 128;
const MAX_STRING_ID: usize = 100;

/// WebSocket close codes this link uses.
pub(super) const NORMAL: u16 = 1000;
pub(super) const PROTOCOL_ERROR: u16 = 1002;
pub(super) const UNSUPPORTED_DATA: u16 = 1003;
pub(super) const POLICY_VIOLATION: u16 = 1008;
pub(super) const TOO_BIG: u16 = 1009;

fn id_valid(id: &Value) -> bool {
    id.as_str()
        .is_some_and(|s| !s.is_empty() && s.len() <= MAX_STRING_ID)
        || id
            .as_i64()
            .is_some_and(|n| (-MAX_SAFE_ID..=MAX_SAFE_ID).contains(&n))
}

/// A well-formed request, notification or response, or the close code refusing it.
pub(super) fn decode(text: &str) -> Result<Value, u16> {
    if text.len() > MAX_FRAME {
        return Err(TOO_BIG);
    }
    let v: Value = serde_json::from_str(text).map_err(|_| PROTOCOL_ERROR)?;
    if !v.is_object() || v["jsonrpc"] != "2.0" {
        return Err(PROTOCOL_ERROR);
    }
    let valid = if v.get("method").is_some() {
        call_valid(&v)
    } else {
        response_valid(&v)
    };
    if valid {
        Ok(v)
    } else {
        Err(PROTOCOL_ERROR)
    }
}

fn call_valid(v: &Value) -> bool {
    v["method"]
        .as_str()
        .is_some_and(|s| !s.is_empty() && s.len() <= MAX_METHOD)
        && v.get("result").is_none()
        && v.get("error").is_none()
        && v.get("params").is_none_or(Value::is_object)
        && v.get("id").is_none_or(id_valid)
}

fn response_valid(v: &Value) -> bool {
    v.get("id").is_some_and(id_valid) && v.get("result").is_some() != v.get("error").is_some()
}

pub(super) fn error(id: Value, code: i64, key: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":key}})
}

/// The frame as a text message, unless it exceeds the bound.
pub(super) fn text(v: Value) -> Option<Message> {
    let s = serde_json::to_string(&v).ok()?;
    (s.len() <= MAX_FRAME).then(|| Message::Text(s.into()))
}

pub(super) fn close(code: u16) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: "".into(),
    }))
}
