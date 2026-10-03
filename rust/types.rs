//! Application values shared across the call, room and transport boundaries.
//! Validation and catalogue resolution belong to the model owner in T2.

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
}

#[derive(Clone, Debug)]
pub struct CallIds {
    pub room_id: String,
    pub session_id: String,
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub struct TranscriptResult {
    pub session_id: String,
    pub request_id: String,
    pub text: String,
    pub language: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SpeechResult {
    pub utterance_id: String,
    pub revision: u64,
    pub audio: Vec<u8>,
    pub mime_type: String,
    pub alignment: Option<Value>,
}
