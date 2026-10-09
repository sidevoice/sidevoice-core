//! Fixtures shared by the models tests.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    models::{default_settings, device_models},
    types::{CallSettings, DeviceModels},
};

/// A device's report as a client builds it from sidevoice-engine's `models()`, with fields the core ignores.
pub(super) fn report_json() -> Value {
    json!({"version": 1, "models": [
        {"id": "whisper-base", "family": "whisper", "capabilities": ["stt"], "languages": ["en", "es", "fr", "de"],
         "installed": false, "recommended_build": "whisper-base/transformers-js-q8",
         "builds": [
            {"id": "whisper-base/transformers-js-q8", "backend": "transformers-js", "accelerator": "wasm",
             "precision": "q8", "available": true, "reasons": []},
            {"id": "whisper-base/mlx-fp16", "backend": "mlx", "precision": "fp16", "available": false,
             "reasons": ["backend_unavailable"]}]},
        {"id": "whisper-tiny", "family": "whisper", "capabilities": ["stt"], "languages": ["en", "es", "fr"],
         "installed": true,
         "builds": [{"id": "whisper-tiny/sherpa-onnx-int8", "backend": "sherpa-onnx", "accelerator": "cpu",
                     "available": true}]},
        {"id": "kokoro-82m-v1.0", "family": "kokoro", "capabilities": ["tts"], "languages": ["en-US", "en-GB", "es"],
         "installed": true,
         "voices": [
            {"id": "ef_dora", "languages": ["es"], "gender": "female"},
            {"id": "em_alex", "languages": ["es"]},
            {"id": "af_heart", "languages": ["en-US"]},
            {"id": "bf_emma", "languages": ["en-GB"]}],
         "builds": [{"id": "kokoro-82m-v1.0/sherpa-onnx-int8", "backend": "sherpa-onnx", "accelerator": "cpu",
                     "available": true}]}
    ]})
}

pub(super) fn report() -> Arc<DeviceModels> {
    device_models(Some(&report_json())).unwrap().unwrap()
}

/// The settings a device gets when it reports `report_json` and no language.
pub(super) fn defaults() -> CallSettings {
    default_settings(None, Some(report()))
}
