use crate::providers::openai::wire::{is_transcription_model, parse_transcription};
use crate::providers::ProviderErrorKind;

#[test]
fn transcription_models_exclude_realtime_live_and_unsafe_ids() {
    for id in [
        "whisper-1",
        "gpt-4o-transcribe",
        "gpt-4o-mini-transcribe",
        "gpt-4o-transcribe-diarize",
    ] {
        assert!(is_transcription_model(id), "{id} should be listed");
    }
    for id in [
        "gpt-4o",
        "whisper-2",
        "gpt-4o-realtime-transcribe",
        "gpt-4o-live-transcribe",
        "-transcribe",
        "gpt 4o transcribe",
        &format!("{}transcribe", "a".repeat(111)),
    ] {
        assert!(!is_transcription_model(id), "{id} should not be listed");
    }
    assert!(is_transcription_model(&format!(
        "{}transcribe",
        "a".repeat(110)
    )));
}

#[test]
fn transcription_reads_trimmed_text_and_mean_of_present_logprobs() {
    let parsed = parse_transcription(
        br#"{"text":"  hi  ","logprobs":[{"logprob":-1.0},{"logprob":null},{"logprob":-0.5}]}"#,
    )
    .unwrap();
    assert_eq!(parsed.text, "hi");
    assert_eq!(parsed.mean_logprob, Some(-0.75));

    let empty = parse_transcription(br#"{"logprobs":[]}"#).unwrap();
    assert_eq!(empty.text, "");
    assert_eq!(empty.mean_logprob, None);

    assert_eq!(
        parse_transcription(b"not-json").unwrap_err().kind,
        ProviderErrorKind::MalformedResponse
    );
}
