//! Defaults from device capabilities and the system language.

use serde_json::json;

use crate::models::default_settings;

#[test]
fn defaults_use_the_first_catalogue_model_when_capabilities_are_unknown() {
    let settings = default_settings(None, None);
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

    let wasm = json!({"runs":"page", "has":["wasm"]});
    let resolved = default_settings(None, Some(&wasm));
    assert_eq!(resolved.stt.model, "whisper-tiny");
    assert_eq!(resolved.tts.model, "kokoro-82m-v1.0");
}

#[test]
fn defaults_normalize_system_language_separately_for_ui_and_speech_catalogues() {
    let spanish = default_settings(Some("es-ES"), None);
    assert_eq!(spanish.ui_language, "es");
    assert_eq!(spanish.stt.options["language"], json!("es"));

    let french = default_settings(Some("fr-FR"), None);
    assert_eq!(french.ui_language, "en");
    assert_eq!(french.stt.options["language"], json!("fr"));

    let unsupported = default_settings(Some("ja-JP"), None);
    assert_eq!(unsupported.ui_language, "en");
    assert_eq!(unsupported.stt.options["language"], json!("en"));
}
