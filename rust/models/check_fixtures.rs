//! The embedded model-check spec and the fixed clip or phrase each check uses.

use std::sync::OnceLock;

use serde_json::Value;

use super::json::field_str;

const CHECKS_JSON: &str = include_str!("../../assets/catalog/models/checks/checks.json");
const CHECK_ES_WAV: &[u8] = include_bytes!("../../assets/catalog/models/checks/stt-es.wav");
const CHECK_EN_WAV: &[u8] = include_bytes!("../../assets/catalog/models/checks/stt-en.wav");

static CHECKS: OnceLock<Value> = OnceLock::new();

/// The model-check rules and fixtures, from the catalogue.
pub(super) fn checks() -> &'static Value {
    CHECKS.get_or_init(|| {
        serde_json::from_str(CHECKS_JSON).expect("embedded check rules are valid JSON")
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CheckClip {
    pub audio: &'static [u8],
    pub text: &'static str,
}

/// The language a model check can actually exercise; unsupported/automatic languages use the English fixture.
pub fn check_language<'a>(task: &str, language: Option<&'a str>) -> &'a str {
    let collection = if task == "stt" { "clips" } else { "phrases" };
    let fallback = field_str(checks(), "fallback").unwrap_or("en");
    let supported = checks()
        .get(task)
        .and_then(|entry| entry.get(collection))
        .and_then(Value::as_object)
        .is_some_and(|entries| language.is_some_and(|language| entries.contains_key(language)));
    if supported {
        language.unwrap()
    } else {
        fallback
    }
}

/// The bundled transcription clip and its reference text for a supported language.
pub fn stt_check_clip(language: Option<&str>) -> CheckClip {
    let language = check_language("stt", language);
    let clip = checks()
        .get("stt")
        .and_then(|entry| entry.get("clips"))
        .and_then(|clips| clips.get(language))
        .expect("the selected transcription check is in the embedded spec");
    let audio = if language == "es" {
        CHECK_ES_WAV
    } else {
        CHECK_EN_WAV
    };
    CheckClip {
        audio,
        text: field_str(clip, "text").expect("check text is present"),
    }
}

/// The fixed phrase a voice model is asked to speak for a supported language.
pub fn tts_check_phrase(language: Option<&str>) -> &'static str {
    let language = check_language("tts", language);
    checks()
        .get("tts")
        .and_then(|entry| entry.get("phrases"))
        .and_then(|phrases| phrases.get(language))
        .and_then(Value::as_str)
        .expect("the selected voice check phrase is in the embedded spec")
}
