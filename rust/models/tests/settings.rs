//! Field-by-field settings validation and its diagnostics.

use serde_json::{json, Map, Value};

use super::support::defaults;
use crate::models::settings_from;

#[test]
fn incoming_wire_settings_validate_ui_locale_exactly_and_fallback_at_field_boundaries() {
    let defaults = defaults();
    let input = json!({
        "ui_language":"es-ES",
        "stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"es", "context":"Sidevoice"}},
        "tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"es":"em_alex"}, "speed":9}},
        "audio_grace_seconds":3,
        "unknown_future_key":1
    });
    let loaded = settings_from(Some(&input), &defaults);
    assert_eq!(loaded.settings.ui_language, "en");
    assert_eq!(loaded.settings.audio_grace_seconds, 3.0);
    assert_eq!(loaded.settings.stt.place, "openai");
    assert_eq!(loaded.settings.stt.options["context"], json!("Sidevoice"));
    assert_eq!(loaded.settings.tts.model, defaults.tts.model);
    assert_eq!(
        loaded.issue.as_deref(),
        Some("Some device settings were not valid and use their defaults: ui_language Input should be 'es' or 'en'; tts Value error, speed: a number from 0.5 to 2")
    );

    assert!(settings_from(None, &defaults).issue.is_none());
    assert!(settings_from(Some(&json!({"old_setting":true})), &defaults)
        .issue
        .is_none());
}

#[test]
fn settings_diagnostics_for_enum_number_and_required_fields() {
    let defaults = defaults();
    let cases = [
        (
            json!({"turn_patience":"patient"}),
            "Some device settings were not valid and use their defaults: turn_patience Input should be 'fast', 'normal' or 'calm'",
        ),
        (
            json!({"turn_end_mode":"other"}),
            "Some device settings were not valid and use their defaults: turn_end_mode Input should be 'timer' or 'smart_turn'",
        ),
        (
            json!({"audio_grace_seconds":"abc"}),
            "Some device settings were not valid and use their defaults: audio_grace_seconds Input should be a valid number, unable to parse string as a number",
        ),
        (
            json!({"tts":{}}),
            "Some device settings were not valid and use their defaults: tts.place Field required; tts.model Field required",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            settings_from(Some(&input), &defaults).issue.as_deref(),
            Some(expected),
            "Settings diagnostic for {input}"
        );
    }

    let mut spanish_defaults = defaults;
    spanish_defaults.ui_language = "es".to_owned();
    assert_eq!(
        settings_from(Some(&json!({"turn_patience":"patient"})), &spanish_defaults)
            .issue
            .as_deref(),
        Some("Algunos ajustes del dispositivo no eran válidos y usan sus valores predeterminados: turn_patience El valor debe ser 'fast', 'normal' o 'calm'")
    );
}

#[test]
fn inclusive_float_endpoints_are_accepted() {
    // Both inclusive endpoints are accepted for every numeric settings field.
    let endpoints = [
        ("audio_grace_seconds", 0.0, 10.0),
        ("replay_on_return_seconds", 0.0, 3600.0),
        ("user_speech_timeout", 0.5, 15.0),
        ("smart_turn_min_silence", 0.1, 3.0),
        ("smart_turn_max_silence", 0.5, 15.0),
        ("vad_confidence", 0.1, 1.0),
        ("vad_min_volume", 0.0, 1.0),
        ("vad_start_secs", 0.05, 1.0),
        ("merge_window_secs", 0.0, 5.0),
    ];
    let defaults = defaults();
    for (name, minimum, maximum) in endpoints {
        for endpoint in [minimum, maximum] {
            let mut input = Map::new();
            input.insert(name.to_owned(), json!(endpoint));
            let loaded = settings_from(Some(&Value::Object(input)), &defaults);
            assert!(loaded.issue.is_none(), "{name}={endpoint}");
        }
    }

    for (name, invalid, expected) in [
        (
            "vad_confidence",
            0.09,
            "Some device settings were not valid and use their defaults: vad_confidence Input should be greater than or equal to 0.1",
        ),
        (
            "audio_grace_seconds",
            11.0,
            "Some device settings were not valid and use their defaults: audio_grace_seconds Input should be less than or equal to 10",
        ),
    ] {
        let mut input = Map::new();
        input.insert(name.to_owned(), json!(invalid));
        assert_eq!(
            settings_from(Some(&Value::Object(input)), &defaults)
                .issue
                .as_deref(),
            Some(expected),
            "Settings diagnostic for {name}"
        );
    }
}

#[test]
fn invalid_stage_is_atomic_while_other_fields_and_stage_survive() {
    let defaults = defaults();
    let input = json!({
        "stt":{"place":"openai", "model":"gpt-4o-transcribe"},
        "tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"es":"em_alex"}, "speed":3}},
        "vad_confidence":2
    });
    let loaded = settings_from(Some(&input), &defaults);
    assert_eq!(loaded.settings.stt.place, "openai");
    assert_eq!(loaded.settings.tts.place, defaults.tts.place);
    assert_eq!(loaded.settings.tts.model, defaults.tts.model);
    assert_eq!(loaded.settings.tts.options, defaults.tts.options);
    assert_eq!(loaded.settings.vad_confidence, defaults.vad_confidence);
    assert_eq!(
        loaded.issue.as_deref(),
        Some("Some device settings were not valid and use their defaults: tts Value error, speed: a number from 0.5 to 2; vad_confidence Input should be less than or equal to 1")
    );
}
