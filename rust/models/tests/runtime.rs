//! The transcription runtime a client reports, and the call's transcription view.

use serde_json::{json, Value};

use super::support::{defaults, report};
use crate::models::{browser_runtime, call_transcription};

#[test]
fn browser_runtime_is_validated_against_the_reported_models() {
    let report = report();
    let report = Some(&report);
    assert_eq!(browser_runtime(None, report), Ok(None));
    assert_eq!(browser_runtime(Some(&json!("webgpu")), report), Ok(None));
    let runtime = browser_runtime(
        Some(&json!({
            "session_id": "ignored", "model": "whisper-base", "engine": "transformers-js",
            "accelerator": "webgpu", "cached": true
        })),
        report,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        Value::Object(runtime.clone()),
        json!({"model":"whisper-base","engine":"transformers-js","accelerator":"webgpu","cached":true})
    );
    let fell_back = browser_runtime(Some(&json!({
        "model": "whisper-base", "engine": "transformers-js", "accelerator": "wasm",
        "cached": "yes", "fallback_from": "webgpu-and-much-more-text", "fallback_error": "x".repeat(400)
    })), report)
    .unwrap()
    .unwrap();
    assert_eq!(fell_back["cached"], false);
    assert_eq!(fell_back["fallback_from"], "webgpu-and-much-more");
    assert_eq!(fell_back["fallback_error"].as_str().unwrap().len(), 300);
    for refused in [
        json!({}),
        json!({"model":"kokoro-82m-v1.0","engine":"transformers-js","accelerator":"wasm"}),
        json!({"model":"whisper-base","engine":"other","accelerator":"wasm"}),
        json!({"model":"whisper-base","engine":"mlx","accelerator":"metal"}),
        json!({"model":"whisper-base","engine":"transformers-js","accelerator":""}),
        json!({"model":"whisper-base","engine":"transformers-js","accelerator":"a".repeat(41)}),
    ] {
        assert_eq!(
            browser_runtime(Some(&refused), report).unwrap_err().key,
            "voice.transcription_runtime_unsupported",
            "{refused}"
        );
    }
    let known = json!({"model":"whisper-base","engine":"transformers-js","accelerator":"wasm"});
    assert_eq!(
        browser_runtime(Some(&known), None).unwrap_err().key,
        "voice.transcription_runtime_unsupported",
        "a device that reported no models runs none"
    );

    let mut stage = defaults().stt;
    stage.options.insert("language".into(), json!("auto"));
    stage
        .options
        .insert("context".into(), json!("private words"));
    let view = call_transcription(&stage, Some(&runtime));
    assert_eq!(view["place"], "device");
    assert_eq!(view["language"], Value::Null);
    assert_eq!(view["engine"], "transformers-js");
    assert!(view.get("context").is_none());
}
