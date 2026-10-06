//! Default call settings from the device's reported capabilities and system language.

use serde_json::{Map, Value};

use super::{
    catalog::{catalog, find_model, model_schema, speech_catalogue_language, task_for_model},
    json::{field_str, strings, values},
    offers::offers,
    options::normalized_default,
};
use crate::{messages::ui_locale, types::CallSettings, types::SpeechStage};

/// Build settings defaults from the device's reported capabilities and language.
/// UI locales are only `en` and `es`; speech-language options retain the catalogue's wider set.
pub fn default_settings(
    system_language: Option<&str>,
    device_capabilities: Option<&Value>,
) -> CallSettings {
    let speech_language = system_language.map(normalize_speech_language);
    let ui_language = system_language.map(ui_locale).unwrap_or("en").to_owned();
    let stt = default_stage("stt", device_capabilities, speech_language.as_deref());
    let tts = default_stage("tts", device_capabilities, speech_language.as_deref());
    CallSettings {
        stt,
        tts,
        ui_language,
        turn_patience: "normal".to_owned(),
        turn_end_mode: "smart_turn".to_owned(),
        user_speech_timeout: 2.5,
        smart_turn_min_silence: 0.9,
        smart_turn_max_silence: 3.0,
        vad_confidence: 0.6,
        vad_min_volume: 0.5,
        vad_start_secs: 0.4,
        merge_window_secs: 0.5,
        audio_grace_seconds: 1.0,
        replay_on_return_seconds: 120.0,
    }
}

fn normalize_speech_language(tag: &str) -> String {
    let primary = tag.split('-').next().unwrap_or("").to_ascii_lowercase();
    if speech_catalogue_language(&primary) {
        primary
    } else {
        "en".to_owned()
    }
}

/// The device's first offered model for `task`, else the catalogue's first, with its option defaults.
fn default_stage(task: &str, capabilities: Option<&Value>, language: Option<&str>) -> SpeechStage {
    let chosen = capabilities
        .and_then(|capabilities| offers(catalog(), capabilities, "device").ok())
        .and_then(|offers| offers.into_iter().find(|offer| offer.task == task))
        .map(|offer| offer.model);
    let model = chosen
        .or_else(|| {
            catalog()
                .get("models")
                .into_iter()
                .flat_map(values)
                .find(|model| task_for_model(model) == Some(task))
                .and_then(|model| field_str(model, "id"))
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let model_entry = find_model(&model).expect("default model exists in embedded catalogue");
    let schema = model_schema(model_entry);
    let mut options = Map::new();
    for option in values(schema) {
        let id = field_str(option, "id").unwrap_or("");
        if field_str(option, "kind") == Some("language")
            && language.is_some_and(|language| {
                strings(option.get("values")).any(|candidate| candidate == language)
            })
        {
            options.insert(id.to_owned(), Value::String(language.unwrap().to_owned()));
        } else if let Some(default) = option.get("default") {
            options.insert(id.to_owned(), normalized_default(option, default));
        }
    }
    SpeechStage {
        place: "device".to_owned(),
        model,
        options,
        build: None,
    }
}
