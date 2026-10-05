//! Incoming call settings: each field is validated alone and falls back to its default when invalid.

use serde_json::{Map, Value};

use super::{
    stage::{parse_stage, StageFailure},
    stage_diagnostics::stage_diagnostics,
};
use crate::{
    messages::{render, LocalizedMessage},
    types::CallSettings,
};

#[derive(Clone, Debug)]
pub struct SettingsLoad {
    pub settings: CallSettings,
    /// Existing clients receive the rendered reason string, not an internal message object.
    pub issue: Option<String>,
}

/// One invalid field, at its wire path, with the reason pinned to Python's settings.py.
#[derive(Clone, Debug)]
pub(super) struct SettingDiagnostic {
    path: String,
    message: LocalizedMessage,
}

impl SettingDiagnostic {
    pub(super) fn new(path: impl Into<String>, message: LocalizedMessage) -> Self {
        Self {
            path: path.into(),
            message,
        }
    }
}

/// The order Python reports fields in, which is the settings model's declaration order.
const FIELD_ORDER: [&str; 14] = [
    "ui_language",
    "stt",
    "tts",
    "audio_grace_seconds",
    "replay_on_return_seconds",
    "turn_patience",
    "turn_end_mode",
    "user_speech_timeout",
    "smart_turn_min_silence",
    "smart_turn_max_silence",
    "vad_confidence",
    "vad_min_volume",
    "vad_start_secs",
    "merge_window_secs",
];

type FloatSetting = fn(&mut CallSettings) -> &mut f32;

/// Every numeric field with its inclusive bounds.
const FLOAT_FIELDS: [(&str, f64, f64, FloatSetting); 9] = [
    ("audio_grace_seconds", 0.0, 10.0, |s| {
        &mut s.audio_grace_seconds
    }),
    ("replay_on_return_seconds", 0.0, 3600.0, |s| {
        &mut s.replay_on_return_seconds
    }),
    ("user_speech_timeout", 0.5, 15.0, |s| {
        &mut s.user_speech_timeout
    }),
    ("smart_turn_min_silence", 0.1, 3.0, |s| {
        &mut s.smart_turn_min_silence
    }),
    ("smart_turn_max_silence", 0.5, 15.0, |s| {
        &mut s.smart_turn_max_silence
    }),
    ("vad_confidence", 0.1, 1.0, |s| &mut s.vad_confidence),
    ("vad_min_volume", 0.0, 1.0, |s| &mut s.vad_min_volume),
    ("vad_start_secs", 0.05, 1.0, |s| &mut s.vad_start_secs),
    ("merge_window_secs", 0.0, 5.0, |s| &mut s.merge_window_secs),
];

/// Validate incoming settings, falling back only the fields that failed and whole stages as one field.
pub fn settings_from(input: Option<&Value>, defaults: &CallSettings) -> SettingsLoad {
    let mut settings = defaults.clone();
    let Some(object) = input
        .and_then(Value::as_object)
        .filter(|object| !object.is_empty())
    else {
        return SettingsLoad {
            settings,
            issue: None,
        };
    };
    let mut invalid = Vec::<SettingDiagnostic>::new();
    choice_fields(object, &mut settings, &mut invalid);
    for (name, min, max, field) in FLOAT_FIELDS {
        let value = field(&mut settings);
        *value = float_field(object, name, *value, min, max, &mut invalid);
    }
    stage_fields(object, &mut settings, &mut invalid);

    invalid.sort_by_key(|diagnostic| {
        let root = diagnostic
            .path
            .split('.')
            .next()
            .unwrap_or(&diagnostic.path);
        FIELD_ORDER
            .iter()
            .position(|candidate| *candidate == root)
            .unwrap_or(FIELD_ORDER.len())
    });
    let issue = issue(&invalid, &settings.ui_language);
    SettingsLoad { settings, issue }
}

fn choice_fields(
    object: &Map<String, Value>,
    settings: &mut CallSettings,
    invalid: &mut Vec<SettingDiagnostic>,
) {
    if let Some(value) = object.get("ui_language") {
        match value.as_str() {
            Some("en") => settings.ui_language = "en".to_owned(),
            Some("es") => settings.ui_language = "es".to_owned(),
            _ => invalid.push(SettingDiagnostic::new(
                "ui_language",
                LocalizedMessage::new("settings.ui_language_invalid"),
            )),
        }
    }
    if let Some(value) = object.get("turn_patience") {
        match value.as_str() {
            Some(value @ ("fast" | "normal" | "calm")) => settings.turn_patience = value.to_owned(),
            _ => invalid.push(SettingDiagnostic::new(
                "turn_patience",
                LocalizedMessage::new("settings.turn_patience_invalid"),
            )),
        }
    }
    if let Some(value) = object.get("turn_end_mode") {
        match value.as_str() {
            Some(value @ ("timer" | "smart_turn")) => settings.turn_end_mode = value.to_owned(),
            _ => invalid.push(SettingDiagnostic::new(
                "turn_end_mode",
                LocalizedMessage::new("settings.turn_end_mode_invalid"),
            )),
        }
    }
}

fn float_field(
    object: &Map<String, Value>,
    name: &str,
    default: f32,
    min: f64,
    max: f64,
    invalid: &mut Vec<SettingDiagnostic>,
) -> f32 {
    let Some(value) = object.get(name) else {
        return default;
    };
    match numeric_value(value) {
        Some(value) if value >= min && value <= max => value as f32,
        Some(value) if value < min => {
            invalid.push(SettingDiagnostic::new(
                name,
                LocalizedMessage::new("settings.input_greater_equal")
                    .with_param("bound", min.to_string()),
            ));
            default
        }
        Some(_) => {
            invalid.push(SettingDiagnostic::new(
                name,
                LocalizedMessage::new("settings.input_less_equal")
                    .with_param("bound", max.to_string()),
            ));
            default
        }
        _ => {
            let key = if value.is_string() {
                "settings.input_number_parse"
            } else {
                "settings.input_number"
            };
            invalid.push(SettingDiagnostic::new(name, LocalizedMessage::new(key)));
            default
        }
    }
}

fn numeric_value(value: &Value) -> Option<f64> {
    let value = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.parse().ok()?,
        _ => return None,
    };
    value.is_finite().then_some(value)
}

/// Each stage is one field: it is replaced whole or kept at its default.
fn stage_fields(
    object: &Map<String, Value>,
    settings: &mut CallSettings,
    invalid: &mut Vec<SettingDiagnostic>,
) {
    for task in ["stt", "tts"] {
        let Some(value) = object.get(task) else {
            continue;
        };
        match parse_stage(task, value) {
            Ok(stage) if task == "stt" => settings.stt = stage,
            Ok(stage) => settings.tts = stage,
            Err(StageFailure::Field(field)) => {
                invalid.extend(stage_diagnostics(task, &field, value))
            }
            Err(StageFailure::Option(option)) => {
                invalid.push(SettingDiagnostic::new(task, option.message()))
            }
        }
    }
}

/// The rendered reason for existing clients, naming at most the first three invalid fields.
fn issue(invalid: &[SettingDiagnostic], ui_language: &str) -> Option<String> {
    if invalid.is_empty() {
        return None;
    }
    let details = invalid
        .iter()
        .take(3)
        .map(|diagnostic| {
            format!(
                "{} {}",
                diagnostic.path,
                render(&diagnostic.message, ui_language)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(render(
        &LocalizedMessage::new("settings.invalid").with_param("details", details),
        ui_language,
    ))
}
