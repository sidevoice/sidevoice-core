//! Application values shared across the call, room and transport boundaries.
//! Validation and catalogue resolution belong to the model owner in T2.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelBuild {
    pub engine: String,
    pub accelerator: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpeechStage {
    pub place: String,
    pub model: String,
    #[serde(default)]
    pub options: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<ModelBuild>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CallSettings {
    pub stt: SpeechStage,
    pub tts: SpeechStage,
    pub ui_language: String,
    pub turn_patience: String,
    pub turn_end_mode: String,
    pub user_speech_timeout: f32,
    pub smart_turn_min_silence: f32,
    pub smart_turn_max_silence: f32,
    pub vad_confidence: f32,
    pub vad_min_volume: f32,
    pub vad_start_secs: f32,
    pub merge_window_secs: f32,
    pub audio_grace_seconds: f32,
    pub replay_on_return_seconds: f32,
    /// The models and builds the device reported for this call, against which its device stages were read.
    /// `None` when it reported none. Never serialized: it is the device's report, not a setting.
    #[serde(skip)]
    pub device_models: Option<Arc<DeviceModels>>,
}

/// The models and builds a device offers, as it reported them (from sidevoice-engine's `models()`), kept to what
/// the core reads. Ids are the engine's own.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DeviceModels {
    pub models: Vec<DeviceModel>,
    /// The model the device chose as its default for each task.
    #[serde(default)]
    pub defaults: DeviceDefaults,
}

/// The device's default model per task, by id; `None` when it has none for that task.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DeviceDefaults {
    #[serde(default)]
    pub stt: Option<String>,
    #[serde(default)]
    pub tts: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct DeviceModel {
    pub id: String,
    /// The tasks it serves: "stt", "tts".
    pub capabilities: Vec<String>,
    /// BCP 47 tags.
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub voices: Vec<DeviceVoice>,
    pub builds: Vec<DeviceBuild>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct DeviceVoice {
    pub id: String,
    /// BCP 47 tags.
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct DeviceBuild {
    /// The backend that runs it: what a stage's `build.engine` names.
    pub backend: String,
    /// The accelerator it would run on there, when the device says.
    #[serde(default)]
    pub accelerator: Option<String>,
    pub available: bool,
}
