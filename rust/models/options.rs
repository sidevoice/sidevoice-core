//! Validation of a stage's options against the catalogue option schema of its model or provider.

use std::collections::HashMap;

use serde_json::{Map, Number, Value};

use super::{
    catalog::speech_catalogue_language,
    json::{field_str, strings, values},
};
use crate::messages::LocalizedMessage;

/// The characters of a refused value shown in its diagnostic, as JSON, before it is cut short.
const SHOWN_LIMIT: usize = 40;

/// A refused value as its diagnostic shows it: its JSON, cut to `SHOWN_LIMIT` characters ending in `…`.
fn shown(value: &Value) -> String {
    let json = value.to_string();
    if json.chars().count() <= SHOWN_LIMIT {
        json
    } else {
        json.chars().take(SHOWN_LIMIT - 1).collect::<String>() + "…"
    }
}

/// The first option that failed, carried as data until the settings boundary renders it.
#[derive(Clone, Debug)]
pub(super) struct OptionFailure {
    name: String,
    kind: OptionFailureKind,
}

#[derive(Clone, Debug)]
enum OptionFailureKind {
    Unknown,
    Language { shown: String },
    TextInvalid { max: usize },
    Range(Box<(Value, Value)>),
    VoiceShape,
    VoiceSpeechLanguage { shown: String },
    ModelVoice { shown: String },
    VoiceLanguage { shown: String, language: String },
    ProviderVoice,
    Other,
}

impl OptionFailure {
    pub(super) fn message(&self) -> LocalizedMessage {
        match &self.kind {
            OptionFailureKind::Unknown => LocalizedMessage::new("settings.option_unknown")
                .with_param("name", self.name.clone()),
            OptionFailureKind::Language { shown } => {
                LocalizedMessage::new("settings.option_language_invalid")
                    .with_param("name", self.name.clone())
                    .with_param("value", shown.clone())
            }
            OptionFailureKind::TextInvalid { max } => {
                LocalizedMessage::new("settings.option_text_too_long")
                    .with_param("name", self.name.clone())
                    .with_param("max", *max)
            }
            OptionFailureKind::Range(bounds) => LocalizedMessage::new("settings.stage_range")
                .with_param("name", self.name.clone())
                .with_param("min", bounds.0.clone())
                .with_param("max", bounds.1.clone()),
            OptionFailureKind::VoiceShape => LocalizedMessage::new("settings.option_voice_shape")
                .with_param("name", self.name.clone())
                .with_param("example", "{language: voice}"),
            OptionFailureKind::VoiceSpeechLanguage { shown } => {
                LocalizedMessage::new("settings.option_voice_speech_language")
                    .with_param("name", self.name.clone())
                    .with_param("value", shown.clone())
            }
            OptionFailureKind::ModelVoice { shown } => {
                LocalizedMessage::new("settings.option_model_voice_invalid")
                    .with_param("name", self.name.clone())
                    .with_param("value", shown.clone())
            }
            OptionFailureKind::VoiceLanguage { shown, language } => {
                LocalizedMessage::new("settings.option_voice_language")
                    .with_param("name", self.name.clone())
                    .with_param("voice", shown.clone())
                    .with_param("language", language.clone())
            }
            OptionFailureKind::ProviderVoice => {
                LocalizedMessage::new("settings.option_provider_voice_invalid")
                    .with_param("name", self.name.clone())
                    .with_param("max", 120)
            }
            OptionFailureKind::Other => LocalizedMessage::new("settings.stage_invalid"),
        }
    }

    fn new(name: impl Into<String>, kind: OptionFailureKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// An option's catalogue default, with range defaults stored as JSON floats.
pub(super) fn normalized_default(option: &Value, default: &Value) -> Value {
    if field_str(option, "kind") == Some("range") {
        default
            .as_f64()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| default.clone())
    } else {
        default.clone()
    }
}

/// The given options checked against `schema`, with defaults filled in for those not given.
pub(super) fn validate_options(
    schema: &Value,
    given: &Map<String, Value>,
    model: Option<&Value>,
) -> Result<Map<String, Value>, OptionFailure> {
    let mut known = HashMap::new();
    for option in values(schema) {
        if let Some(id) = field_str(option, "id") {
            known.insert(id, option);
        }
    }
    let mut unknown = given
        .keys()
        .filter(|key| !known.contains_key(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unknown.sort();
    if let Some(first) = unknown.first() {
        return Err(OptionFailure::new(
            first.clone(),
            OptionFailureKind::Unknown,
        ));
    }

    let mut result = Map::new();
    for option in values(schema) {
        let Some(id) = field_str(option, "id") else {
            continue;
        };
        if let Some(value) = given.get(id) {
            result.insert(id.to_owned(), option_value(option, id, value, model)?);
        } else if let Some(default) = option.get("default") {
            result.insert(id.to_owned(), normalized_default(option, default));
        }
    }
    Ok(result)
}

fn option_value(
    option: &Value,
    name: &str,
    value: &Value,
    model: Option<&Value>,
) -> Result<Value, OptionFailure> {
    match field_str(option, "kind") {
        Some("language") => language_value(option, name, value),
        Some("text") => text_value(option, name, value),
        Some("range") => range_value(option, name, value),
        Some("voice") => voice_value(option, name, value, model),
        _ => Err(OptionFailure::new(name, OptionFailureKind::Other)),
    }
}

fn language_value(option: &Value, name: &str, value: &Value) -> Result<Value, OptionFailure> {
    if value.as_str().is_some_and(|language| {
        strings(option.get("values")).any(|candidate| candidate == language)
            || (language == "auto" && option.get("auto").and_then(Value::as_bool) == Some(true))
    }) {
        Ok(value.clone())
    } else {
        Err(OptionFailure::new(
            name,
            OptionFailureKind::Language {
                shown: shown(value),
            },
        ))
    }
}

fn text_value(option: &Value, name: &str, value: &Value) -> Result<Value, OptionFailure> {
    let max = option.get("max").and_then(Value::as_u64).unwrap_or(1000) as usize;
    if value
        .as_str()
        .is_some_and(|text| text.chars().count() <= max)
    {
        Ok(value.clone())
    } else {
        Err(OptionFailure::new(
            name,
            OptionFailureKind::TextInvalid { max },
        ))
    }
}

fn range_value(option: &Value, name: &str, value: &Value) -> Result<Value, OptionFailure> {
    let minimum = option.get("min").cloned().unwrap_or(Value::Null);
    let maximum = option.get("max").cloned().unwrap_or(Value::Null);
    let range_failure = || {
        OptionFailure::new(
            name,
            OptionFailureKind::Range(Box::new((minimum.clone(), maximum.clone()))),
        )
    };
    let Some(number) = value.as_f64() else {
        return Err(range_failure());
    };
    let (Some(min), Some(max)) = (minimum.as_f64(), maximum.as_f64()) else {
        return Err(range_failure());
    };
    if !number.is_finite() || !(min..=max).contains(&number) {
        return Err(range_failure());
    }
    Number::from_f64(number)
        .map(Value::Number)
        .ok_or_else(range_failure)
}

fn voice_value(
    option: &Value,
    name: &str,
    value: &Value,
    model: Option<&Value>,
) -> Result<Value, OptionFailure> {
    if option.get("per_language").and_then(Value::as_bool) != Some(true) {
        return Ok(Value::String(voice_id(option, name, value, model, None)?));
    }
    let Some(voices) = value.as_object() else {
        return Err(OptionFailure::new(name, OptionFailureKind::VoiceShape));
    };
    let mut result = Map::new();
    for (language, voice) in voices {
        if !speech_catalogue_language(language) {
            return Err(OptionFailure::new(
                name,
                OptionFailureKind::VoiceSpeechLanguage {
                    shown: shown(&Value::String(language.clone())),
                },
            ));
        }
        result.insert(
            language.clone(),
            Value::String(voice_id(option, name, voice, model, Some(language))?),
        );
    }
    Ok(Value::Object(result))
}

fn voice_id(
    option: &Value,
    name: &str,
    voice: &Value,
    model: Option<&Value>,
    language: Option<&str>,
) -> Result<String, OptionFailure> {
    if field_str(option, "from") == Some("model.voices") {
        model_voice_id(name, voice, model, language)
    } else if voice
        .as_str()
        .is_some_and(|voice| !voice.trim().is_empty() && voice.chars().count() <= 120)
    {
        Ok(voice.as_str().unwrap().to_owned())
    } else {
        Err(OptionFailure::new(name, OptionFailureKind::ProviderVoice))
    }
}

/// A voice the model lists, which must speak `language` when one is given.
fn model_voice_id(
    name: &str,
    voice: &Value,
    model: Option<&Value>,
    language: Option<&str>,
) -> Result<String, OptionFailure> {
    let found = voice.as_str().and_then(|voice_id| {
        model
            .and_then(|model| model.get("voices"))
            .into_iter()
            .flat_map(values)
            .find(|candidate| field_str(candidate, "id") == Some(voice_id))
    });
    let Some(found) = found else {
        return Err(OptionFailure::new(
            name,
            OptionFailureKind::ModelVoice {
                shown: shown(voice),
            },
        ));
    };
    let voice = voice.as_str().expect("matched catalogue voice is a string");
    if let Some(language) = language {
        let speaks = field_str(found, "language").unwrap_or("").split('-').next() == Some(language);
        if !speaks {
            return Err(OptionFailure::new(
                name,
                OptionFailureKind::VoiceLanguage {
                    shown: shown(&Value::String(voice.to_owned())),
                    language: language.to_owned(),
                },
            ));
        }
    }
    Ok(voice.to_owned())
}
