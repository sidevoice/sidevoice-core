//! Verdicts on a trial's input audio and on the transcript a provider returned for it.

use std::collections::HashSet;

/// 20 ms frames of 16 kHz, 16-bit PCM.
const FRAME_BYTES: usize = 640;
const AUDIBLE_RMS: f64 = 0.008;
/// A quarter second of audible samples at 16 kHz.
const MIN_AUDIBLE_SAMPLES: usize = 16_000 / 4;

/// Whether 16 kHz little-endian 16-bit PCM holds at least a quarter second of audible frames.
pub(super) fn enough_speech(pcm: &[u8]) -> bool {
    let audible = pcm
        .chunks(FRAME_BYTES)
        .map(|frame| {
            let rms = (frame
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| {
                    let value = i16::from_le_bytes(*bytes) as f64 / 32768.0;
                    value * value
                })
                .sum::<f64>()
                / (frame.len() / 2) as f64)
                .sqrt();
            if rms >= AUDIBLE_RMS {
                frame.len() / 2
            } else {
                0
            }
        })
        .sum::<usize>();
    audible >= MIN_AUDIBLE_SAMPLES
}

/// Whether a transcript reads as speech rather than a run of punctuation, a few repeated characters
/// or a loop of the same words.
pub(super) fn usable(text: &str) -> bool {
    let trimmed = text.trim();
    let compact = trimmed
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if trimmed.is_empty()
        || (!trimmed.chars().any(char::is_alphanumeric) && compact.chars().count() >= 8)
    {
        return false;
    }
    if compact.chars().count() >= 24 && compact.chars().collect::<HashSet<_>>().len() <= 3 {
        return false;
    }
    let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
    if tokens.len() >= 10
        && tokens.iter().copied().collect::<HashSet<_>>().len() as f64 / (tokens.len() as f64)
            < 0.15
    {
        return false;
    }
    true
}
