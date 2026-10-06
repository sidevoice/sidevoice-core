//! The attributes a collector may receive.

use serde_json::{Map, Value};

/// Ids and names are bounded like every other string the room keeps.
pub(super) const MAX_VALUE: usize = 200;

pub(super) const ALLOWED: [&str; 24] = [
    "sidevoice.session_id",
    "sidevoice.thread_id",
    "sidevoice.turn_revision",
    "sidevoice.reply_revision",
    "sidevoice.utterance_id",
    "sidevoice.status",
    "sidevoice.reason",
    "sidevoice.outcome",
    "sidevoice.kind",
    "sidevoice.stt_place",
    "sidevoice.stt_model",
    "sidevoice.stt_accelerator",
    "sidevoice.tts_provider",
    "sidevoice.tts_model",
    "sidevoice.turn_end_mode",
    "sidevoice.harness",
    "sidevoice.shared_audio",
    "sidevoice.synthesis_attempt",
    "sidevoice.stage",
    "sidevoice.duration_ms",
    "sidevoice.audio_output",
    "sidevoice.audio_context",
    "sidevoice.stalls",
    "sidevoice.build_id",
];

/// Exact Python allowlist, with strings bounded before they reach a collector.
pub(super) fn attributes(values: &Value) -> Value {
    let mut kept = Map::new();
    if let Some(values) = values.as_object() {
        for (key, value) in values {
            if !ALLOWED.contains(&key.as_str()) || value.is_null() {
                continue;
            }
            let value = match value {
                Value::String(text) => Value::String(text.chars().take(MAX_VALUE).collect()),
                Value::Bool(_) | Value::Number(_) => value.clone(),
                _ => continue,
            };
            kept.insert(key.clone(), value);
        }
    }
    Value::Object(kept)
}
