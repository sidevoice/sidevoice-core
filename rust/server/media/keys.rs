//! Cloud provider credentials: the stored integration key, else the environment's.

use serde_json::Value;

use crate::storage::PrivateDir;

pub(in crate::server) fn provider_key(dir: &PrivateDir, name: &str) -> Option<String> {
    let stored = dir
        .read_json("integrations.json")
        .ok()
        .flatten()
        .and_then(|value| value.get(name).and_then(Value::as_str).map(str::to_owned));
    let environment = match name {
        "openai" => "VOICE_STT_API_KEY",
        "elevenlabs" => "VOICE_ELEVENLABS_API_KEY",
        _ => return None,
    };
    crate::models::effective_key(
        stored.as_deref(),
        std::env::var(environment).ok().as_deref(),
    )
    .map(str::to_owned)
}
