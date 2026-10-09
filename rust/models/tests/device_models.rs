//! Reading a device's report of its models, and device stages read against it.

use serde_json::json;

use super::support::{defaults, report, report_json};
use crate::models::{default_settings, device_models, settings_from};

#[test]
fn a_report_is_read_with_the_engine_ids_and_without_the_fields_core_ignores() {
    assert_eq!(device_models(None), Ok(None));
    assert_eq!(device_models(Some(&json!(null))), Ok(None));
    let report = report();
    let ids = report
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["whisper-base", "whisper-tiny", "kokoro-82m-v1.0"]);
    let base = &report.models[0];
    assert_eq!(base.builds[0].backend, "transformers-js");
    assert_eq!(base.builds[0].accelerator.as_deref(), Some("wasm"));
    assert!(!base.builds[1].available);
    assert_eq!(report.models[2].voices[2].languages, ["en-US"]);
}

#[test]
fn an_unreadable_report_is_refused_with_its_key() {
    let mut other_version = report_json();
    other_version["version"] = json!(2);
    let mut no_version = report_json();
    no_version.as_object_mut().unwrap().remove("version");
    let mut bad_id = report_json();
    bad_id["models"][0]["id"] = json!("bad/id");
    let mut no_builds = report_json();
    no_builds["models"][0]
        .as_object_mut()
        .unwrap()
        .remove("builds");
    let mut long_backend = report_json();
    long_backend["models"][0]["builds"][0]["backend"] = json!("b".repeat(121));
    let mut too_many = report_json();
    too_many["models"] = json!(vec![report_json()["models"][1].clone(); 65]);
    for refused in [
        json!("models"),
        json!({"version": 1}),
        other_version,
        no_version,
        bad_id,
        no_builds,
        long_backend,
        too_many,
    ] {
        assert_eq!(
            device_models(Some(&refused)).unwrap_err().key,
            "voice.device_models_unsupported"
        );
    }
}

#[test]
fn device_stages_must_name_a_reported_model_and_an_available_backend() {
    let defaults = defaults();
    let accepted = json!({
        "stt": {"place": "device", "model": "whisper-base", "options": {"language": "fr"},
                "build": {"engine": "transformers-js", "accelerator": "wasm"}},
        "tts": {"place": "device", "model": "kokoro-82m-v1.0",
                "options": {"voice": {"es": "em_alex", "en": "bf_emma"}, "speed": 1.5},
                "build": {"engine": "sherpa-onnx", "accelerator": "cpu"}}
    });
    let loaded = settings_from(Some(&accepted), &defaults);
    assert_eq!(loaded.issue, None);
    assert_eq!(loaded.settings.stt.model, "whisper-base");
    assert_eq!(loaded.settings.tts.options["voice"]["en"], "bf_emma");

    for (input, reason) in [
        (
            json!({"stt": {"place": "device", "model": "whisper-base",
                           "build": {"engine": "mlx", "accelerator": "metal"}}}),
            "stt Input should be a valid stage",
        ),
        (
            json!({"stt": {"place": "device", "model": "whisper-large-v3"}}),
            "stt Value error, 'whisper-large-v3' is not a stt model this device offers",
        ),
        (
            json!({"stt": {"place": "device", "model": "whisper-tiny", "options": {"language": "de"}}}),
            "stt Value error, language: \"de\" is not one of its languages",
        ),
    ] {
        let loaded = settings_from(Some(&input), &defaults);
        assert_eq!(
            loaded.issue.as_deref(),
            Some(
                format!("Some device settings were not valid and use their defaults: {reason}")
                    .as_str()
            ),
            "{input}"
        );
        assert_eq!(loaded.settings.stt.model, defaults.stt.model);
    }
}

#[test]
fn a_device_that_reported_nothing_offers_no_device_model() {
    let nothing = default_settings(None, None);
    let stored = json!({"stt": {"place": "device", "model": "whisper-tiny"}});
    let loaded = settings_from(Some(&stored), &nothing);
    assert_eq!(
        loaded.issue.as_deref(),
        Some("Some device settings were not valid and use their defaults: stt Value error, 'whisper-tiny' is not a stt model this device offers")
    );
    let provider = json!({"stt": {"place": "openai", "model": "gpt-4o-transcribe"}});
    assert_eq!(settings_from(Some(&provider), &nothing).issue, None);
}
