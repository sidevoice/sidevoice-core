//! The flat refusal shape sent on the wire.

use serde_json::{Map, Value};

use super::{render, LocalizedMessage};

/// Render the existing flat refusal shape, keeping message parameters at the top level.
///
/// Only fields of the refusal contract are copied. Internal rendering parameters such as
/// `provider_label` and `seconds_display` are never exposed on the wire.
pub fn render_refusal(message: &LocalizedMessage, language: &str) -> Map<String, Value> {
    let mut refusal = Map::new();
    refusal.insert("key".to_owned(), Value::String(message.key.clone()));
    for field in wire_fields(&message.key) {
        if let Some(value) = message.params.get(*field) {
            refusal.insert((*field).to_owned(), value.clone());
        }
    }
    refusal.insert(
        "message".to_owned(),
        Value::String(render(message, language)),
    );
    refusal
}

/// The parameters of `key` that belong to its wire contract.
fn wire_fields(key: &str) -> &'static [&'static str] {
    match key {
        "provider_key_missing"
        | "voice_missing"
        | "provider_key_refused"
        | "provider_unreachable" => &["provider"],
        "provider_failed" => &["provider", "detail"],
        "check_mismatch" => &["heard"],
        "check_invalid" => &["details"],
        "check_duration" => &["seconds"],
        "check_rate_limited" => &["retry_after", "scope"],
        _ => &[],
    }
}
