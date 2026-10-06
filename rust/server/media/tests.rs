//! Unit tests of the call media helpers; turn ownership is tested under `turns`.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::{
    audio::{to_gate_rate, wav, GATE_RATE},
    recognition::{accepted, recognition_language},
    speech::{attach_audio, describe_voice},
    transcripts::DeviceTranscripts,
};
use crate::{
    models::ResolvedVoice,
    providers::{cache::CachedSpeech, CloudSpeech},
    types::CallSettings,
};

fn settings(language: Option<&str>) -> CallSettings {
    let mut settings = crate::models::default_settings(None, None);
    settings.ui_language = "en".into();
    settings.stt.options.clear();
    if let Some(language) = language {
        settings
            .stt
            .options
            .insert("language".into(), json!(language));
    }
    settings
}

#[test]
fn wav_frames_mono_16_bit_pcm_and_refuses_partial_samples() {
    let pcm: Vec<u8> = [0_i16, 1000, -1000]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    let bytes = wav(&pcm, 24_000).unwrap();
    let reader = hound::WavReader::new(std::io::Cursor::new(bytes)).unwrap();
    let spec = reader.spec();
    assert_eq!(
        (spec.channels, spec.sample_rate, spec.bits_per_sample),
        (1, 24_000, 16)
    );
    let samples: Vec<i16> = reader.into_samples::<i16>().map(Result::unwrap).collect();
    assert_eq!(samples, vec![0, 1000, -1000]);
    assert!(wav(&[], 16_000).is_none());
    assert!(wav(&[1, 2, 3], 16_000).is_none());
}

#[test]
fn gate_copy_keeps_16_khz_audio_and_resamples_other_rates() {
    let pcm: Vec<u8> = (0..4_800_i16)
        .flat_map(|i| (i % 200).to_le_bytes())
        .collect();
    assert_eq!(to_gate_rate(&pcm, GATE_RATE), pcm);
    let resampled = to_gate_rate(&pcm, 48_000);
    assert!(resampled.len().is_multiple_of(2));
    assert!(!resampled.is_empty() && resampled.len() < pcm.len());
}

#[test]
fn transcripts_outside_the_expected_script_or_confidence_are_dropped() {
    let english = settings(None);
    assert_eq!(
        accepted("hello there", None, &english).as_deref(),
        Some("hello there")
    );
    assert_eq!(accepted("", None, &english), None);
    assert_eq!(accepted("привет", None, &english), None);
    assert_eq!(accepted("123", None, &english).as_deref(), Some("123"));
    let hindi = settings(Some("hi"));
    assert_eq!(accepted("नमस्ते", None, &hindi).as_deref(), Some("नमस्ते"));
    // Two words or fewer may be less likely than longer transcripts.
    assert_eq!(
        accepted("ok then", Some(-2.5), &english).as_deref(),
        Some("ok then")
    );
    assert_eq!(accepted("ok then", Some(-3.5), &english), None);
    assert_eq!(accepted("one two three", Some(-2.5), &english), None);
    assert_eq!(
        accepted("one two three", Some(-1.5), &english).as_deref(),
        Some("one two three")
    );
}

#[tokio::test]
async fn device_transcripts_answer_only_their_session_and_request() {
    let transcripts = DeviceTranscripts::default();
    let mut reply = transcripts.open("request");
    transcripts.resolve(
        &json!({"session_id":"other","request_id":"request","text":"no"}),
        false,
        "session",
    );
    assert!(reply.try_recv().is_err());
    transcripts.resolve(
        &json!({"session_id":"session","request_id":"request","text":"  hello  "}),
        false,
        "session",
    );
    assert_eq!(reply.await.unwrap(), Ok(Some("hello".into())));

    let blank = transcripts.open("blank");
    transcripts.resolve(
        &json!({"session_id":"session","request_id":"blank","text":"   "}),
        false,
        "session",
    );
    assert_eq!(blank.await.unwrap(), Ok(None));

    let failed = transcripts.open("failed");
    transcripts.resolve(
        &json!({"session_id":"session","request_id":"failed"}),
        true,
        "session",
    );
    assert_eq!(failed.await.unwrap(), Err(()));

    let forgotten = transcripts.open("forgotten");
    transcripts.forget("forgotten");
    assert!(forgotten.await.is_err());
    let cleared = transcripts.open("cleared");
    transcripts.clear();
    assert!(cleared.await.is_err());
}

#[test]
fn speech_message_carries_voice_then_audio_and_shares_no_timings() {
    let voice = ResolvedVoice {
        place: "elevenlabs".into(),
        model: "model".into(),
        voice: "voice".into(),
        language: "en".into(),
        speed: 1.0,
    };
    let mut timings = Map::new();
    timings.insert("request_to_complete_ms".into(), json!(12.0));
    let speech = Arc::new(CloudSpeech {
        audio: vec![1, 2, 3],
        mime_type: "audio/mpeg".into(),
        alignment: None,
        timings_ms: timings.clone(),
    });
    let mut object = Map::new();
    describe_voice(&mut object, &voice);
    attach_audio(
        &mut object,
        &CachedSpeech {
            speech: speech.clone(),
            fresh: true,
        },
    );
    assert_eq!(
        Value::Object(object).to_string(),
        json!({"place":"elevenlabs","model":"model","voice":"voice","speed":1.0,"language":"en",
            "mime_type":"audio/mpeg","audio_base64":"AQID","alignment":null,
            "timings_ms":timings,"shared":false})
        .to_string()
    );

    let mut shared = Map::new();
    attach_audio(
        &mut shared,
        &CachedSpeech {
            speech,
            fresh: false,
        },
    );
    assert_eq!(shared["timings_ms"], json!({}));
    assert_eq!(shared["shared"], json!(true));
}

#[test]
fn automatic_language_is_not_sent_to_the_recogniser() {
    assert_eq!(recognition_language(&settings(Some("auto"))), None);
    assert_eq!(recognition_language(&settings(Some("es"))), Some("es"));
    assert_eq!(recognition_language(&settings(None)), None);
}
