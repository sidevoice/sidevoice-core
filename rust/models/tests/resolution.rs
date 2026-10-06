//! Whether chosen settings can run, and which voice a reply uses.

use serde_json::json;

use super::support::defaults;
use crate::models::{resolve_voice, settings_from, unavailable};

#[test]
fn availability_refusals_keep_host_precedence_then_provider_key_then_voice() {
    let defaults = defaults();
    let host_input = json!({"tts":{"place":"host", "model":"kokoro-82m-v1.0"}});
    let host = settings_from(Some(&host_input), &defaults).settings;
    assert_eq!(
        unavailable(&host, |_| false).unwrap().key,
        "place_host_unavailable"
    );

    let provider_input =
        json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{}}}});
    let provider = settings_from(Some(&provider_input), &defaults).settings;
    assert_eq!(
        unavailable(&provider, |_| false).unwrap().key,
        "provider_key_missing"
    );
    let missing_voice = unavailable(&provider, |_| true).unwrap();
    assert_eq!(missing_voice.key, "voice_missing");
    assert_eq!(missing_voice.params["provider"], json!("elevenlabs"));
    assert!(unavailable(&defaults, |_| true).is_none());
}

#[test]
fn reply_voice_resolution_uses_model_language_then_provider_selection_order() {
    let defaults = defaults();
    let spanish_voice = resolve_voice(&defaults, Some("es")).unwrap();
    assert_eq!(spanish_voice.voice, "ef_dora");
    assert_eq!(
        resolve_voice(&defaults, Some("en")).unwrap().voice,
        "af_heart"
    );

    let input = json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{
        "voice":{"es":"voz-espanola", "en":"english-voice"}, "speed":1.1
    }}});
    let settings = settings_from(Some(&input), &defaults).settings;
    assert_eq!(
        resolve_voice(&settings, Some("en")).unwrap().voice,
        "english-voice"
    );
    let fallback = resolve_voice(&settings, Some("fr")).unwrap();
    assert_eq!(fallback.voice, "voz-espanola");
    assert_eq!(fallback.speed, 1.1);
    assert_eq!(resolve_voice(&settings, None).unwrap().language, "en");
    assert_eq!(
        resolve_voice(&settings, Some("ja")).unwrap_err().key,
        "speech_language_unsupported"
    );
}
