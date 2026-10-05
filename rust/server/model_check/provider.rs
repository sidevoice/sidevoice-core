//! Two passes of a paid provider on the fixed check clip or phrase, judged by the shared model-check verdicts.

use std::time::Instant;

use serde_json::{json, Value};

use crate::messages::LocalizedMessage;
use crate::models::{
    audio_problem, check_language, slow, stt_check_clip, transcript_problem, tts_check_phrase,
};
use crate::providers::{ElevenLabsTts, OpenAiTranscriber, ProviderError, ProviderErrorKind};
use crate::types::SpeechStage;

const PASSES: usize = 2;

pub(super) fn refusal(message: LocalizedMessage, language: &str) -> Value {
    Value::Object(crate::messages::render_refusal(&message, language))
}

pub(super) fn provider_label(place: &str) -> &'static str {
    if place == "openai" {
        "OpenAI"
    } else {
        "ElevenLabs"
    }
}

fn failed(step: &str, reason: Value, passes: Vec<Value>) -> Value {
    json!({"ok":false,"step":step,"reason":reason,"passes":passes})
}

/// The failed step (`key` or `check`) and its refusal for a provider error.
fn provider_problem(place: &str, error: &ProviderError, language: &str) -> (&'static str, Value) {
    let (step, message) = match error.kind {
        ProviderErrorKind::Unauthorized => ("key", LocalizedMessage::new("provider_key_refused")),
        ProviderErrorKind::Timeout | ProviderErrorKind::Transport => {
            ("check", LocalizedMessage::new("provider_unreachable"))
        }
        _ => (
            "check",
            LocalizedMessage::new("provider_failed").with_param(
                "detail",
                error
                    .status
                    .map_or_else(|| format!("{:?}", error.kind), |status| status.to_string()),
            ),
        ),
    };
    (
        step,
        refusal(
            message
                .with_param("provider", place.to_owned())
                .with_param("provider_label", provider_label(place)),
            language,
        ),
    )
}

fn provider_failed(place: &str, error: &ProviderError, ui: &str, passes: Vec<Value>) -> Value {
    let (step, reason) = provider_problem(place, error, ui);
    failed(step, reason, passes)
}

pub(super) async fn check_provider(
    task: &str,
    stage: SpeechStage,
    language: Option<String>,
    key: String,
    ui: String,
) -> Value {
    if task == "stt" {
        check_transcription(stage, language, &key, &ui).await
    } else {
        check_speech(stage, language, &key, &ui).await
    }
}

async fn check_transcription(
    stage: SpeechStage,
    language: Option<String>,
    key: &str,
    ui: &str,
) -> Value {
    let own = stage
        .options
        .get("language")
        .and_then(Value::as_str)
        .filter(|value| *value != "auto");
    let chosen = check_language("stt", own.or(language.as_deref()));
    let clip = stt_check_clip(Some(chosen));
    let client = match OpenAiTranscriber::new(key) {
        Ok(client) => client,
        Err(error) => return provider_failed(&stage.place, &error, ui, vec![]),
    };
    let context = stage
        .options
        .get("context")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let mut passes = Vec::new();
    for _ in 0..PASSES {
        let start = Instant::now();
        let result = client
            .transcribe(clip.audio, &stage.model, Some(chosen), context)
            .await;
        let transcript = match result {
            Ok(transcript) => transcript,
            Err(error) => return provider_failed(&stage.place, &error, ui, passes),
        };
        let latency = start.elapsed().as_millis() as u64;
        passes.push(json!({"latency_ms":latency,"text":transcript.text}));
        if let Some(problem) = transcript_problem(clip.text, &transcript.text) {
            return failed("check", refusal(problem, ui), passes);
        }
    }
    let latency = passes
        .last()
        .and_then(|pass| pass["latency_ms"].as_u64())
        .unwrap_or(0);
    json!({"ok":true,"step":"done","language":chosen,"passes":passes,
        "latency_ms":latency,"slow":slow(latency)})
}

async fn check_speech(stage: SpeechStage, language: Option<String>, key: &str, ui: &str) -> Value {
    let chosen = check_language("tts", language.as_deref());
    let voice = stage.options.get("voice").and_then(|value| match value {
        Value::Object(voices) => voices
            .get(chosen)
            .or_else(|| voices.values().next())
            .and_then(Value::as_str),
        _ => value.as_str(),
    });
    let Some(voice) = voice.filter(|voice| !voice.is_empty()) else {
        return failed(
            "check",
            refusal(
                LocalizedMessage::new("voice_missing")
                    .with_param("provider", stage.place.clone())
                    .with_param("provider_label", "ElevenLabs"),
                ui,
            ),
            vec![],
        );
    };
    let client = match ElevenLabsTts::new(key) {
        Ok(client) => client,
        Err(error) => return provider_failed(&stage.place, &error, ui, vec![]),
    };
    let phrase = tts_check_phrase(Some(chosen));
    let speed = stage
        .options
        .get("speed")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let mut passes = Vec::new();
    for _ in 0..PASSES {
        let result = client
            .synthesize(phrase, &stage.model, voice, speed, false, "pcm_16000")
            .await;
        let speech = match result {
            Ok(speech) => speech,
            Err(error) => return provider_failed(&stage.place, &error, ui, passes),
        };
        let samples = pcm16_samples(&speech.audio);
        passes.push(speech_pass(&speech.timings_ms, samples.len()));
        if let Some(problem) = audio_problem(&samples, 16_000.0) {
            return failed("check", refusal(problem, ui), passes);
        }
    }
    let latency = passes
        .last()
        .and_then(|pass| pass["first_audio_ms"].as_u64())
        .unwrap_or(0);
    json!({"ok":true,"step":"done","language":chosen,"voice":voice,"passes":passes,
        "latency_ms":latency,"slow":false})
}

/// Little-endian 16-bit PCM as samples in [-1, 1).
fn pcm16_samples(audio: &[u8]) -> Vec<f32> {
    audio
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.0)
        .collect()
}

/// One synthesis pass's report, from the provider's timings and the 16 kHz sample count.
fn speech_pass(timings_ms: &serde_json::Map<String, Value>, samples: usize) -> Value {
    let total = timings_ms
        .get("request_to_complete_ms")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let first = timings_ms
        .get("request_to_first_chunk_ms")
        .and_then(Value::as_f64)
        .unwrap_or(total);
    let seconds = samples as f64 / 16_000.0;
    json!({"first_audio_ms":first.round() as u64,"total_ms":total.round() as u64,
    "audio_seconds":(seconds*100.0).round()/100.0,
    "realtime":if total>0.0 {Some((seconds*100_000.0/total).round()/100.0)} else {None}})
}
