//! Model-check fixtures and verdicts against the embedded spec.

use serde_json::{json, Value};

use crate::models::{
    audio_problem, check_fixtures::checks, check_language, slow, stt_check_clip,
    transcript_problem, tts_check_phrase, word_error,
};

#[test]
fn model_check_fixtures_and_transcript_verdicts_follow_existing_rules() {
    let spanish = stt_check_clip(Some("es"));
    assert_eq!(spanish.text, tts_check_phrase(Some("es")));
    assert!(!spanish.audio.is_empty());
    assert_eq!(check_language("stt", Some("auto")), "en");
    assert_eq!(check_language("tts", Some("fr")), "en");
    assert_eq!(word_error(spanish.text, &spanish.text.to_uppercase()), 0.0);
    let unaccented = " hola esto es una prueba de transcripcion para comprobar que el modelo entiende lo que digo";
    assert!(transcript_problem(spanish.text, unaccented).is_none());
    assert_eq!(
        transcript_problem(spanish.text, "... ").unwrap().key,
        "check_silent"
    );
    let mismatch = transcript_problem(spanish.text, "Thank you for watching.").unwrap();
    assert_eq!(mismatch.key, "check_mismatch");
    assert_eq!(mismatch.params["heard"], json!("Thank you for watching."));
}

#[test]
fn model_check_audio_verdicts_and_latency_cutoff_match_the_embedded_spec() {
    let tone = (0..80_000)
        .map(|index| (0.3 * (index as f64 / 10.0).sin()) as f32)
        .collect::<Vec<_>>();
    assert!(audio_problem(&tone, 16_000.0).is_none());
    assert_eq!(
        audio_problem(&vec![0.0; 80_000], 16_000.0).unwrap().key,
        "check_silent"
    );
    assert_eq!(
        audio_problem(&tone[..1_600], 16_000.0).unwrap().key,
        "check_duration"
    );
    let duration = audio_problem(&tone[..1_920], 16_000.0).unwrap();
    assert_eq!(duration.params["seconds"], json!(0.12));
    assert_eq!(duration.params["seconds_display"], json!("0.1"));
    assert_eq!(
        Value::Object(crate::messages::render_refusal(&duration, "en")),
        json!({
            "key":"check_duration",
            "seconds":0.12,
            "message":"The model produced 0.1 s of audio for a phrase that takes about five."
        })
    );
    assert_eq!(
        audio_problem(&tone, f64::INFINITY).unwrap().key,
        "check_invalid_audio"
    );
    let mut not_finite = tone.clone();
    not_finite[777] = f32::NAN;
    assert_eq!(
        audio_problem(&not_finite, 16_000.0).unwrap().key,
        "check_invalid_audio"
    );

    let comfort = checks()["stt"]["comfort_ms"].as_u64().unwrap();
    assert!(!slow(comfort));
    assert!(slow(comfort + 1));
}
