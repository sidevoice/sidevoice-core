//! The room's microphone detector values from the device's one-word turn patience.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::defaults::default_settings;
use crate::{
    messages::{render, LocalizedMessage},
    types::CallSettings,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MicSettings {
    pub turn_end_mode: String,
    pub user_speech_timeout: f32,
    pub smart_turn_min_silence: f32,
    pub smart_turn_max_silence: f32,
    pub vad_confidence: f32,
    pub vad_min_volume: f32,
    pub vad_start_secs: f32,
    pub merge_window_secs: f32,
}

impl MicSettings {
    /// The call's settings with this detector tuning in place of whatever the device sent or stored.
    /// A device chooses its patience and nothing else about turn detection (see `mic_settings`).
    pub fn applied_to(&self, settings: &CallSettings) -> CallSettings {
        CallSettings {
            turn_end_mode: self.turn_end_mode.clone(),
            user_speech_timeout: self.user_speech_timeout,
            smart_turn_min_silence: self.smart_turn_min_silence,
            smart_turn_max_silence: self.smart_turn_max_silence,
            vad_confidence: self.vad_confidence,
            vad_min_volume: self.vad_min_volume,
            vad_start_secs: self.vad_start_secs,
            merge_window_secs: self.merge_window_secs,
            ..settings.clone()
        }
    }
}

/// Convert the device's one-word patience choice to the room's microphone detector values.
pub fn mic_settings(
    settings: &CallSettings,
    overrides: Option<&Value>,
) -> (MicSettings, Option<String>) {
    let defaults = default_settings(None, None);
    let base = MicSettings {
        turn_end_mode: defaults.turn_end_mode,
        user_speech_timeout: defaults.user_speech_timeout,
        smart_turn_min_silence: defaults.smart_turn_min_silence,
        smart_turn_max_silence: defaults.smart_turn_max_silence,
        vad_confidence: defaults.vad_confidence,
        vad_min_volume: defaults.vad_min_volume,
        vad_start_secs: defaults.vad_start_secs,
        merge_window_secs: defaults.merge_window_secs,
    };
    let patience_override = overrides
        .and_then(Value::as_object)
        .and_then(|object| object.get("turn_patience"))
        .and_then(Value::as_str);
    let patience = patience_override.unwrap_or(&settings.turn_patience);
    let mut effective = base.clone();
    match patience {
        "fast" => {
            effective.smart_turn_min_silence = 0.6;
            effective.smart_turn_max_silence = 2.5;
            effective.user_speech_timeout = 2.0;
            effective.merge_window_secs = 0.0;
            (effective, None)
        }
        "normal" => (effective, None),
        "calm" => {
            effective.smart_turn_min_silence = 1.3;
            effective.smart_turn_max_silence = 4.0;
            effective.user_speech_timeout = 3.5;
            effective.merge_window_secs = 1.5;
            (effective, None)
        }
        _ => {
            let shown = patience.chars().take(40).collect::<String>();
            (
                base,
                Some(render(
                    &LocalizedMessage::new("turn_patience_unknown").with_param("patience", shown),
                    &settings.ui_language,
                )),
            )
        }
    }
}
