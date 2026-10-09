//! The models and builds a device reports it offers, and the rules its device stages are read with.
//!
//! The device reports them in its call hello (`device_models`) and again in `voice-device-models` when they change.
//! A device that reports none offers no device models.

use std::sync::{Arc, LazyLock};

use serde_json::{json, Value};

use super::{catalog::speech_languages, stage::valid_model_id};
use crate::{
    messages::LocalizedMessage,
    types::{DeviceModel, DeviceModels},
};

/// The only report version this core reads.
const VERSION: u64 = 1;
const MAX_MODELS: usize = 64;
const MAX_BUILDS: usize = 16;
const MAX_VOICES: usize = 256;
const MAX_LANGUAGES: usize = 256;
const MAX_TEXT: usize = 120;

static NOTHING: LazyLock<DeviceModels> = LazyLock::new(DeviceModels::default);

/// The device's report, or `None` when it sent none. `Err` is a report this core cannot read.
pub fn device_models(value: Option<&Value>) -> Result<Option<Arc<DeviceModels>>, LocalizedMessage> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => read(value)
            .map(|models| Some(Arc::new(models)))
            .ok_or_else(|| LocalizedMessage::new("voice.device_models_unsupported")),
    }
}

fn read(value: &Value) -> Option<DeviceModels> {
    if value.get("version").and_then(Value::as_u64) != Some(VERSION) {
        return None;
    }
    let models = DeviceModels {
        models: serde_json::from_value(value.get("models")?.clone()).ok()?,
    };
    within_limits(&models).then_some(models)
}

fn within_limits(report: &DeviceModels) -> bool {
    let text = |value: &str| !value.is_empty() && value.chars().count() <= MAX_TEXT;
    report.models.len() <= MAX_MODELS
        && report.models.iter().all(|model| {
            valid_model_id(&model.id)
                && model.capabilities.iter().all(|task| text(task))
                && model.languages.len() <= MAX_LANGUAGES
                && model.languages.iter().all(|language| text(language))
                && model.voices.len() <= MAX_VOICES
                && model.voices.iter().all(|voice| {
                    text(&voice.id)
                        && voice.languages.len() <= MAX_LANGUAGES
                        && voice.languages.iter().all(|language| text(language))
                })
                && model.builds.len() <= MAX_BUILDS
                && model.builds.iter().all(|build| {
                    text(&build.backend) && build.accelerator.as_deref().is_none_or(text)
                })
        })
}

/// The report device stages are read against: the device's own, else one that offers nothing.
pub(super) fn offered(report: Option<&Arc<DeviceModels>>) -> &DeviceModels {
    report.map_or(&*NOTHING, |report| report.as_ref())
}

/// The reported model `model_id` when it serves `task`.
pub(super) fn model_for<'a>(
    report: &'a DeviceModels,
    task: &str,
    model_id: &str,
) -> Option<&'a DeviceModel> {
    report
        .models
        .iter()
        .find(|model| model.id == model_id)
        .filter(|model| serves(model, task))
}

pub(super) fn serves(model: &DeviceModel, task: &str) -> bool {
    model
        .capabilities
        .iter()
        .any(|capability| capability == task)
}

/// Whether one of the model's builds that runs on the device uses `backend`.
pub(super) fn runs_on(model: &DeviceModel, backend: &str) -> bool {
    model
        .builds
        .iter()
        .any(|build| build.available && build.backend == backend)
}

/// The primary subtag of a BCP 47 tag, lowercased: "en" for "en-US".
pub(super) fn primary(tag: &str) -> String {
    tag.split('-').next().unwrap_or("").to_ascii_lowercase()
}

/// The option schema of a device stage: the same for every model of a task, its values from the model.
pub(super) fn device_schema(task: &str, model: &DeviceModel) -> Value {
    if task == "stt" {
        let spoken = model
            .languages
            .iter()
            .map(|language| primary(language))
            .collect::<Vec<_>>();
        let values = speech_languages()
            .filter(|language| spoken.iter().any(|spoken| spoken.as_str() == *language))
            .collect::<Vec<_>>();
        let default = if values.contains(&"en") { "en" } else { "auto" };
        json!([{"id": "language", "kind": "language", "values": values, "auto": true, "default": default}])
    } else {
        json!([
            {"id": "voice", "kind": "voice", "from": "model.voices", "per_language": true},
            {"id": "speed", "kind": "range", "min": 0.5, "max": 2, "step": 0.05, "default": 1}
        ])
    }
}
