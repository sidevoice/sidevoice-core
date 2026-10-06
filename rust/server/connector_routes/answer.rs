//! What of the connector's answer about host agents may reach a device.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::control::room::PeerError;

use super::keyed;

const MAX_TEXT: usize = 500;
const MAX_ITEMS: usize = 24;
const MAX_DEPTH: u8 = 4;
const MAX_KEY: usize = 64;
/// Parameter names that may carry raw process output rather than display values.
const UNSAFE_WORDS: [&str; 9] = [
    "command",
    "executable",
    "stderr",
    "stdout",
    "raw",
    "output",
    "message",
    "detail",
    "error",
];

pub(super) fn host_agent_response(answer: Option<Result<Value, PeerError>>) -> Response {
    let answer = match answer {
        None => return keyed(StatusCode::GATEWAY_TIMEOUT, "connector-timeout"),
        Some(Err(_)) => return keyed(StatusCode::BAD_GATEWAY, "connector-unavailable"),
        Some(Ok(answer)) => answer,
    };
    match answer {
        v if v["error"].is_object() => (
            StatusCode::CONFLICT,
            Json(json!({"error":error_body(&v["error"])})),
        )
            .into_response(),
        v if v["agents"].is_array() && v["custom"].is_object() => Json(
            json!({"agents":v["agents"],"scanned_at":v.get("scanned_at"),"custom":v["custom"]}),
        )
        .into_response(),
        _ => keyed(StatusCode::BAD_GATEWAY, "invalid-connector-response"),
    }
}

fn error_body(error: &Value) -> Value {
    let key = error["key"]
        .as_str()
        .filter(|key| valid_connector_error_key(key))
        .unwrap_or("connector-error");
    let mut body = json!({"key":key});
    if let Some(Value::Object(params)) = safe_connector_params(&error["params"], 0) {
        if !params.is_empty() {
            body["params"] = Value::Object(params);
        }
    }
    if let Some(message) = error["message"].as_str() {
        body["message"] = json!(truncated(message));
    }
    body
}

pub(super) fn valid_connector_error_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
        && !bytes
            .windows(2)
            .any(|pair| b"._-".contains(&pair[0]) && b"._-".contains(&pair[1]))
}

pub(super) fn safe_connector_params(value: &Value, depth: u8) -> Option<Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    match value {
        Value::Null | Value::Bool(_) => Some(value.clone()),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(value.clone()),
        Value::String(text) => Some(json!(truncated(text))),
        Value::Array(items) => Some(json!(items
            .iter()
            .take(MAX_ITEMS)
            .filter_map(|item| safe_connector_params(item, depth + 1))
            .collect::<Vec<_>>())),
        Value::Object(items) => {
            let mut clean = Map::new();
            for (key, item) in items.iter().take(MAX_ITEMS) {
                if !safe_key(key) {
                    continue;
                }
                if let Some(item) = safe_connector_params(item, depth + 1) {
                    clean.insert(key.clone(), item);
                }
            }
            Some(Value::Object(clean))
        }
        _ => None,
    }
}

fn safe_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    key.len() <= MAX_KEY
        && !UNSAFE_WORDS
            .iter()
            .any(|unsafe_word| lower.contains(unsafe_word))
}

fn truncated(text: &str) -> String {
    text.chars().take(MAX_TEXT).collect()
}
