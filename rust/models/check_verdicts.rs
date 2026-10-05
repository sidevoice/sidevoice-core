//! Verdicts on a model check's output: transcript accuracy, audio plausibility and latency.

use serde_json::Value;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use super::{check_fixtures::checks, json::values};
use crate::messages::LocalizedMessage;

/// Word error rate after Unicode compatibility decomposition, accent removal and punctuation folding.
pub fn word_error(expected: &str, heard: &str) -> f64 {
    let expected = words(expected);
    let heard = words(heard);
    if expected.is_empty() {
        return if heard.is_empty() { 0.0 } else { 1.0 };
    }
    let mut row = (0..=heard.len()).collect::<Vec<_>>();
    for (i, word) in expected.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, other) in heard.iter().enumerate() {
            let above = row[j + 1];
            let substitution = previous + usize::from(word != other);
            row[j + 1] = (above + 1).min(row[j] + 1).min(substitution);
            previous = above;
        }
    }
    row[heard.len()] as f64 / expected.len() as f64
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .map(|character| {
            if character.is_alphanumeric() || character.is_whitespace() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// A stable refusal when a transcription check is silent or too different from its fixture.
pub fn transcript_problem(expected: &str, heard: &str) -> Option<LocalizedMessage> {
    if words(heard).is_empty() {
        return Some(LocalizedMessage::new("check_silent"));
    }
    let max_error = checks()
        .get("stt")
        .and_then(|entry| entry.get("max_word_error"))
        .and_then(Value::as_f64)
        .unwrap_or(0.5);
    if word_error(expected, heard) > max_error {
        let heard = heard.trim().chars().take(200).collect::<String>();
        Some(LocalizedMessage::new("check_mismatch").with_param("heard", heard))
    } else {
        None
    }
}

/// A stable refusal for malformed, silent or implausibly short/long waveform output.
pub fn audio_problem(samples: &[f32], sample_rate: f64) -> Option<LocalizedMessage> {
    if !sample_rate.is_finite()
        || sample_rate <= 0.0
        || samples.iter().any(|sample| !sample.is_finite())
    {
        return Some(LocalizedMessage::new("check_invalid_audio"));
    }
    let minimum_rms = checks()
        .get("tts")
        .and_then(|entry| entry.get("min_rms"))
        .and_then(Value::as_f64)
        .unwrap_or(0.005);
    if rms(samples) < minimum_rms {
        return Some(LocalizedMessage::new("check_silent"));
    }
    let seconds = samples.len() as f64 / sample_rate;
    let bounds = checks()
        .get("tts")
        .and_then(|entry| entry.get("seconds"))
        .map(values)
        .unwrap_or(&[]);
    let low = bounds.first().and_then(Value::as_f64).unwrap_or(1.5);
    let high = bounds.get(1).and_then(Value::as_f64).unwrap_or(20.0);
    if !(low..=high).contains(&seconds) {
        let rounded = format!("{seconds:.2}").parse::<f64>().unwrap_or(seconds);
        let display = format!("{seconds:.1}");
        return Some(
            LocalizedMessage::new("check_duration")
                .with_param("seconds", rounded)
                .with_param("seconds_display", display),
        );
    }
    None
}

fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples
        .iter()
        .map(|sample| (*sample as f64).powi(2))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt()
}

/// Latency above the comfort line is reported to the person, never treated as a failed model check.
pub fn slow(latency_ms: u64) -> bool {
    let comfort = checks()
        .get("stt")
        .and_then(|entry| entry.get("comfort_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(2000);
    latency_ms > comfort
}
