//! Stage and option rules, and the atomic fallback of a refused stage.

use serde_json::json;

use super::support::defaults;
use crate::models::settings_from;

#[test]
fn stage_rules_reject_cross_task_models_provider_builds_and_unknown_options() {
    let defaults = defaults();
    let cases = [
        (
            json!({"stt":{"place":"device", "model":"kokoro-82m-v1.0"}}),
            "Some device settings were not valid and use their defaults: stt Value error, 'kokoro-82m-v1.0' is not a stt model of the catalogue",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "build":{"engine":"sherpa-onnx", "accelerator":"cpu"}}}),
            "Some device settings were not valid and use their defaults: stt Value error, a provider runs its own models: a build cannot be chosen",
        ),
        (
            json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"pitch":1}}}),
            "Some device settings were not valid and use their defaults: tts Value error, unknown options: 'pitch'",
        ),
        (
            json!({"stt":{"place":"openai", "model":"bad/id"}}),
            "Some device settings were not valid and use their defaults: stt Value error, 'bad/id' is not a model id",
        ),
        (
            json!({"tts":{"place":"device", "model":"missing"}}),
            "Some device settings were not valid and use their defaults: tts Value error, 'missing' is not a tts model of the catalogue",
        ),
    ];
    for (input, expected) in cases {
        let loaded = settings_from(Some(&input), &defaults);
        assert_eq!(
            loaded.issue.as_deref(),
            Some(expected),
            "Settings diagnostic for {input}"
        );
    }
}

#[test]
fn known_option_diagnostics_show_the_refused_value_as_json_and_keep_atomic_stage_fallback() {
    let defaults = defaults();
    let overlong_context = "x".repeat(401);
    let overlong_voice = "x".repeat(121);
    let cases = [
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"de"}}}),
            "language: \"de\" is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":5}}}),
            "language: 5 is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":null}}}),
            "language: null is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":true}}}),
            "language: true is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":["de"]}}}),
            "language: [\"de\"] is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"isn't"}}}),
            "language: \"isn't\" is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"a\nb"}}}),
            "language: \"a\\nb\" is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"\u{200b}"}}}),
            "language: \"\u{200b}\" is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"x".repeat(50)}}}),
            "language: \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx… is not one of its languages",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"context":overlong_context}}}),
            "context: text of at most 400 characters",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"context":7}}}),
            "context: text of at most 400 characters",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"context":["a"]}}}),
            "context: text of at most 400 characters",
            "stt",
        ),
        (
            json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"context":null}}}),
            "context: text of at most 400 characters",
            "stt",
        ),
        (
            json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":"af_heart"}}}),
            "voice: one voice per speech language, as {language: voice}",
            "tts",
        ),
        (
            json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"de":"af_heart"}}}}),
            "voice: \"de\" is not a speech language",
            "tts",
        ),
        (
            json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"en":"no-such-voice"}}}}),
            "voice: \"no-such-voice\" is not a voice of this model",
            "tts",
        ),
        (
            json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"en":5}}}}),
            "voice: 5 is not a voice of this model",
            "tts",
        ),
        (
            json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"en":"em_alex"}}}}),
            "voice: \"em_alex\" does not speak en",
            "tts",
        ),
        (
            json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":"v"}}}),
            "voice: one voice per speech language, as {language: voice}",
            "tts",
        ),
        (
            json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"xx":"v"}}}}),
            "voice: \"xx\" is not a speech language",
            "tts",
        ),
        (
            json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"en":""}}}}),
            "voice: a voice id is a non-empty string of at most 120 characters",
            "tts",
        ),
        (
            json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"en":"   "}}}}),
            "voice: a voice id is a non-empty string of at most 120 characters",
            "tts",
        ),
        (
            json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"en":overlong_voice}}}}),
            "voice: a voice id is a non-empty string of at most 120 characters",
            "tts",
        ),
        (
            json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"en":null}}}}),
            "voice: a voice id is a non-empty string of at most 120 characters",
            "tts",
        ),
    ];
    for (input, reason, task) in cases {
        let expected = format!(
            "Some device settings were not valid and use their defaults: {task} Value error, {reason}"
        );
        let loaded = settings_from(Some(&input), &defaults);
        assert_eq!(
            loaded.issue.as_deref(),
            Some(expected.as_str()),
            "Settings diagnostic for {task} option in {input}"
        );
        if task == "stt" {
            assert_eq!(loaded.settings.stt.place, defaults.stt.place);
            assert_eq!(loaded.settings.stt.model, defaults.stt.model);
            assert_eq!(loaded.settings.stt.options, defaults.stt.options);
        } else {
            assert_eq!(loaded.settings.tts.place, defaults.tts.place);
            assert_eq!(loaded.settings.tts.model, defaults.tts.model);
            assert_eq!(loaded.settings.tts.options, defaults.tts.options);
        }
    }
}
