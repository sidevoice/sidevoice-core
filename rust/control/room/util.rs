//! Small helpers shared by every part of the room: clocks, identifiers, input checks and the
//! default conversation title.
use std::time::{SystemTime, UNIX_EPOCH};

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
