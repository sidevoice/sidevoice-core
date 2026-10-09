//! The reply voice for a spoken language, from the chosen text-to-speech stage.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    catalog::{provider_schema, speech_catalogue_language},
    device_models::{device_schema, model_for, offered, primary},
    json::{field_str, values},
};
use crate::{
    messages::LocalizedMessage,
    types::{CallSettings, DeviceModel},
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ResolvedVoice {
    pub place: String,
    pub model: String,
    pub voice: String,
    pub language: String,
    pub speed: f64,
}

/// Resolve the voice for a reply language, keeping device and provider voice-choice behavior independent.
pub fn resolve_voice(
    settings: &CallSettings,
    language: Option<&str>,
) -> Result<ResolvedVoice, LocalizedMessage> {
    let language = language
        .filter(|language| !language.is_empty())
        .unwrap_or(&settings.ui_language);
    if !speech_catalogue_language(language) {
        return Err(LocalizedMessage::new("speech_language_unsupported")
            .with_param("language", language.to_owned()));
    }
    let stage = &settings.tts;
    if stage.place == "host" {
        return Err(LocalizedMessage::new("place_host_unavailable"));
    }
    let reported = model_for(
        offered(settings.device_models.as_ref()),
        "tts",
        &stage.model,
    );
    let schema = if stage.place == "device" {
        device_schema("tts", reported.ok_or_else(voice_unavailable)?)
    } else {
        provider_schema(&stage.place, "tts").clone()
    };
    let voice_option = values(&schema)
        .iter()
        .find(|option| field_str(option, "kind") == Some("voice"));
    let chosen = stage.options.get("voice");
    let voice =
        if voice_option.is_some_and(|option| field_str(option, "from") == Some("model.voices")) {
            model_voice(reported.ok_or_else(voice_unavailable)?, chosen, language)
        } else {
            provider_voice(chosen, language)
        };
    let Some(voice) = voice.filter(|voice| !voice.is_empty()) else {
        return Err(LocalizedMessage::new("speech_voice_unavailable")
            .with_param("language", language.to_owned()));
    };
    let speed = stage
        .options
        .get("speed")
        .and_then(Value::as_f64)
        .or_else(|| {
            values(&schema)
                .iter()
                .find(|option| field_str(option, "id") == Some("speed"))
                .and_then(|option| option.get("default"))
                .and_then(Value::as_f64)
        })
        .unwrap_or(1.0);
    Ok(ResolvedVoice {
        place: stage.place.clone(),
        model: stage.model.clone(),
        voice: voice.to_owned(),
        language: language.to_owned(),
        speed,
    })
}

fn voice_unavailable() -> LocalizedMessage {
    LocalizedMessage::new("speech_voice_unavailable")
}

/// The voice picked for `language`, or for a single-voice choice, the one voice chosen.
fn picked<'a>(chosen: Option<&'a Value>, language: &str) -> Option<&'a Value> {
    match chosen {
        Some(Value::Object(voices)) => voices.get(language),
        Some(value) => Some(value),
        None => None,
    }
}

/// A reported voice: the picked one if the model lists it for `language`, else the model's first that does.
fn model_voice<'a>(
    model: &'a DeviceModel,
    chosen: Option<&'a Value>,
    language: &str,
) -> Option<&'a str> {
    let spoken = model
        .voices
        .iter()
        .filter(|voice| voice.languages.iter().any(|tag| primary(tag) == language))
        .collect::<Vec<_>>();
    let first_spoken = spoken.first().map(|voice| voice.id.as_str());
    picked(chosen, language)
        .and_then(Value::as_str)
        .filter(|picked| spoken.iter().any(|voice| voice.id == *picked))
        .or(first_spoken)
}

/// A provider voice: the picked one, else the first voice chosen for any language.
fn provider_voice<'a>(chosen: Option<&'a Value>, language: &str) -> Option<&'a str> {
    picked(chosen, language)
        .and_then(Value::as_str)
        .or_else(|| {
            chosen
                .and_then(Value::as_object)
                .and_then(|voices| voices.values().next())
                .and_then(Value::as_str)
        })
}
