use super::*;

#[tokio::test]
async fn contract_slice_preserves_representative_json_shapes() {
    let defaults = presentation_languages().await.0;
    let typed: contract_types_generated::LanguagesResponse =
        serde_json::from_value(defaults.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), defaults);
    assert!(defaults["stt"].as_object().unwrap().contains_key("build"));
    assert!(defaults["stt"]["build"].is_null());
    assert!(defaults["tts"].as_object().unwrap().contains_key("build"));
    assert!(defaults["tts"]["build"].is_null());
    assert_eq!(defaults["replay_on_return_seconds"].as_u64(), Some(120));
    assert_eq!(defaults["smart_turn_min_silence"].as_f64(), Some(0.9));

    let trial = json!({"place":"openai","model":"gpt-4o-transcribe",
        "audio":{"encoding":"pcm_s16le","sample_rate":16000,"data_base64":""}});
    let typed: contract_types_generated::TrialRequest = serde_json::from_value(trial.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), trial);
    assert!(trial.get("options").is_none());
    let refusal = json!({"detail":{"key":"trial.silent","message":"localized"}});
    let typed: contract_types_generated::TrialRefusal =
        serde_json::from_value(refusal.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), refusal);
    let voice_media = json!({"type":"voice-media","data":{"session_id":"session","path":"socket"}});
    let typed: contract_types_generated::VoiceMediaCommand =
        serde_json::from_value(voice_media.clone()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), voice_media);
    assert!(serde_json::from_value::<contract_types_generated::VoiceMediaCommand>(
        json!({"type":"voice-media","data":{"session_id":"session","path":"unknown"}})
    )
    .is_err());
    assert!(serde_json::from_value::<contract_types_generated::VoiceMediaCommand>(
        json!({"type":"voice-unknown","data":{"session_id":"session","path":"socket"}})
    )
    .is_err());
}
