//! Default call settings from the models the device reported and its system language.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::{
    catalog::speech_catalogue_language,
    device_models::{device_schema, primary, serves},
    json::{field_str, strings, values},
    options::normalized_default,
};
use crate::{
    messages::ui_locale,
    types::{CallSettings, DeviceModels, SpeechStage},
};

/// Build settings defaults from the device's reported models and language.
/// UI locales are only `en` and `es`; speech-language options retain the catalogue's wider set.
pub fn default_settings(
    system_language: Option<&str>,
    device_models: Option<Arc<DeviceModels>>,
) -> CallSettings {
    let speech_language = system_language.map(normalize_speech_language);
    let ui_language = system_language.map(ui_locale).unwrap_or("en").to_owned();
    let stt = default_stage("stt", device_models.as_deref(), speech_language.as_deref());
    let tts = default_stage("tts", device_models.as_deref(), speech_language.as_deref());
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
        device_models,
    }
}

fn normalize_speech_language(tag: &str) -> String {
    let primary = primary(tag);
    if speech_catalogue_language(&primary) {
        primary
    } else {
        "en".to_owned()
    }
}

/// The device's first reported model for `task`, an installed one first, with its option defaults. A device that
/// reported none for `task` gets a stage naming no model, which a call refuses to run.
fn default_stage(task: &str, report: Option<&DeviceModels>, language: Option<&str>) -> SpeechStage {
    let candidates = report
        .into_iter()
        .flat_map(|report| &report.models)
        .filter(|model| serves(model, task));
    let chosen = candidates
        .clone()
        .find(|model| model.installed)
        .or_else(|| candidates.clone().next());
    let mut options = Map::new();
    if let Some(model) = chosen {
        let schema = device_schema(task, model);
        for option in values(&schema) {
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
    }
    SpeechStage {
        place: "device".to_owned(),
        model: chosen.map(|model| model.id.clone()).unwrap_or_default(),
        options,
        build: None,
    }
}
