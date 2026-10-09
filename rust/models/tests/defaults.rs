//! Defaults from the device's reported models and the system language.

use serde_json::json;

use super::support::{defaults, report, report_json};
use crate::models::{default_settings, device_models, unavailable};

#[test]
fn defaults_take_the_model_the_device_reports_as_its_default_for_each_task() {
    let settings = defaults();
    assert_eq!(
        (&*settings.stt.place, &*settings.stt.model),
        ("device", "whisper-tiny")
    );
    assert_eq!(
        (&*settings.tts.place, &*settings.tts.model),
        ("device", "kokoro-82m-v1.0")
    );
    assert_eq!(settings.stt.options["language"], json!("en"));
    assert_eq!(settings.tts.options["speed"], json!(1.0));
    assert_eq!(
        (
            settings.ui_language.as_str(),
            settings.turn_patience.as_str()
        ),
        ("en", "normal")
    );

    let spanish_only = json!({"version": 1, "defaults": {"stt": "whisper-small"}, "models": [
        {"id": "whisper-base", "capabilities": ["stt"], "languages": ["en"],
         "builds": [{"backend": "whisper-cpp", "available": true}]},
        {"id": "whisper-small", "capabilities": ["stt"], "languages": ["es"],
         "builds": [{"backend": "whisper-cpp", "available": true}]}]});
    let chosen = default_settings(None, device_models(Some(&spanish_only)).unwrap());
    assert_eq!(chosen.stt.model, "whisper-small");
    assert_eq!(
        chosen.stt.options["language"],
        json!("auto"),
        "a model without English defaults to detection"
    );
    assert_eq!(chosen.tts.model, "", "no default for tts: core picks none");
}

#[test]
fn a_device_that_reports_no_default_for_a_task_gets_a_stage_the_call_refuses() {
    let mut no_defaults = report_json();
    no_defaults.as_object_mut().unwrap().remove("defaults");
    for settings in [
        default_settings(None, None),
        default_settings(
            None,
            device_models(Some(&json!({"version": 1, "models": []}))).unwrap(),
        ),
        default_settings(None, device_models(Some(&no_defaults)).unwrap()),
    ] {
        assert_eq!(settings.stt.place, "device");
        assert_eq!(settings.stt.model, "");
        assert!(settings.tts.options.is_empty());
        assert_eq!(
            unavailable(&settings, |_| true).unwrap().key,
            "device_model_missing"
        );
    }
    assert!(unavailable(&defaults(), |_| true).is_none());
}

#[test]
fn defaults_normalize_system_language_separately_for_ui_and_speech_catalogues() {
    let spanish = default_settings(Some("es-ES"), Some(report()));
    assert_eq!(spanish.ui_language, "es");
    assert_eq!(spanish.stt.options["language"], json!("es"));

    let french = default_settings(Some("fr-FR"), Some(report()));
    assert_eq!(french.ui_language, "en");
    assert_eq!(french.stt.options["language"], json!("fr"));

    let unsupported = default_settings(Some("ja-JP"), Some(report()));
    assert_eq!(unsupported.ui_language, "en");
    assert_eq!(unsupported.stt.options["language"], json!("en"));
}
