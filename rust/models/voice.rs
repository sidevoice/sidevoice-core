//! The reply voice for a spoken language, from the chosen text-to-speech stage.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    catalog::{find_model, model_schema, provider_schema, speech_catalogue_language},
    json::{field_str, values},
};
use crate::{messages::LocalizedMessage, types::CallSettings};

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
    let schema = if stage.place == "device" {
        model_schema(find_model(&stage.model).ok_or_else(voice_unavailable)?)
    } else {
        provider_schema(&stage.place, "tts")
    };
    let voice_option = values(schema)
        .iter()
        .find(|option| field_str(option, "kind") == Some("voice"));
    let chosen = stage.options.get("voice");
    let voice =
        if voice_option.is_some_and(|option| field_str(option, "from") == Some("model.voices")) {
            model_voice(&stage.model, chosen, language)?
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
            values(schema)
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

/// A catalogue voice: the picked one if the model lists it for `language`, else the model's first that does.
fn model_voice<'a>(
    model_id: &str,
    chosen: Option<&'a Value>,
    language: &str,
) -> Result<Option<&'a str>, LocalizedMessage> {
    let model = find_model(model_id).ok_or_else(voice_unavailable)?;
    let spoken = model
        .get("voices")
        .into_iter()
        .flat_map(values)
        .filter(|entry| {
            field_str(entry, "language").unwrap_or("").split('-').next() == Some(language)
        })
        .collect::<Vec<_>>();
    let first_spoken = spoken.first().and_then(|entry| field_str(entry, "id"));
    Ok(picked(chosen, language)
        .and_then(Value::as_str)
        .filter(|picked| {
            spoken
                .iter()
                .any(|entry| field_str(entry, "id") == Some(*picked))
        })
        .or(first_spoken))
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
