//! Whether chosen call settings can run at all, given which provider keys exist.

use serde_json::Value;

use super::{catalog::find_provider, json::field_str};
use crate::{messages::LocalizedMessage, types::CallSettings};

/// Whether the call settings cannot run because the host is not an available model place or a provider is
/// missing a key/voice. The caller supplies only key availability; T2 never reads storage or environment state.
pub fn unavailable(
    settings: &CallSettings,
    provider_key_available: impl Fn(&str) -> bool,
) -> Option<LocalizedMessage> {
    if settings.stt.place == "host" || settings.tts.place == "host" {
        return Some(LocalizedMessage::new("place_host_unavailable"));
    }
    for (task, stage) in [("stt", &settings.stt), ("tts", &settings.tts)] {
        if matches!(stage.place.as_str(), "device" | "host") {
            continue;
        }
        let Some(provider) = find_provider(&stage.place) else {
            continue;
        };
        let refusal = |key: &str| {
            LocalizedMessage::new(key)
                .with_param("provider", stage.place.clone())
                .with_param(
                    "provider_label",
                    field_str(provider, "label")
                        .unwrap_or(&stage.place)
                        .to_owned(),
                )
        };
        if !provider_key_available(&stage.place) {
            return Some(refusal("provider_key_missing"));
        }
        if task == "tts" && voice_is_missing(stage.options.get("voice")) {
            return Some(refusal("voice_missing"));
        }
    }
    None
}

fn voice_is_missing(voice: Option<&Value>) -> bool {
    match voice {
        None | Some(Value::Null) => true,
        Some(Value::String(voice)) => voice.is_empty(),
        Some(Value::Object(voices)) => voices.is_empty(),
        _ => false,
    }
}
