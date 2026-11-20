//! Small helpers shared by every part of the room: clocks, identifiers, input checks and the
//! default conversation title.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(super) fn seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(super) fn millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub(super) fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
pub(super) fn id() -> String {
    Uuid::new_v4().to_string()
}
pub(super) fn field<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}
pub(super) fn valid_thread(thread: &str) -> bool {
    !thread.is_empty()
        && thread.len() <= 200
        && thread
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
/// The title a conversation shows when its agent declared none.
pub(super) fn default_title(thread: &str, language: &str) -> String {
    crate::messages::render(
        &crate::messages::LocalizedMessage::new("room.conversation_title")
            .with_param("id", thread.chars().take(8).collect::<String>()),
        language,
    )
}

/// How long a call whose socket went stays parked for its page to come back.
const RESUME_SECONDS: f64 = 60.0;
/// `VOICE_RESUME_SECONDS` as the core reads it: unreadable or negative is the default. The call's resume window and
/// the age after which offline words are late are this one value.
pub fn resume_window(value: Option<&str>) -> Duration {
    let seconds = value
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .unwrap_or(RESUME_SECONDS);
    Duration::from_secs_f64(seconds)
}
/// The configured resume window.
pub fn configured_resume_window() -> Duration {
    resume_window(std::env::var("VOICE_RESUME_SECONDS").ok().as_deref())
}
