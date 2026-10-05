//! Unit tests of the transcription trial's budget and its audio and transcript verdicts.

use super::budget::TrialBudget;
use super::speech::{enough_speech, usable};

fn pcm(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
    samples.into_iter().flat_map(i16::to_le_bytes).collect()
}

fn tone(samples: usize) -> Vec<u8> {
    pcm((0..samples).map(|index| ((index as f64 / 10.0).sin() * 8_000.0) as i16))
}

#[test]
fn a_quarter_second_of_audible_frames_is_enough_speech() {
    assert!(enough_speech(&tone(4_000)));
    assert!(!enough_speech(&tone(3_999)));
    assert!(!enough_speech(&pcm(std::iter::repeat_n(0, 16_000))));
    let mut mixed = pcm(std::iter::repeat_n(0, 16_000));
    mixed.extend(tone(4_000));
    assert!(enough_speech(&mixed));
}

#[test]
fn transcripts_of_noise_or_loops_are_not_usable() {
    assert!(usable(" Hola, esto es una prueba. "));
    assert!(usable("..."));
    assert!(!usable("   "));
    assert!(!usable("........"));
    assert!(!usable(&"ah".repeat(12)));
    assert!(!usable(&"thank you ".repeat(10).repeat(2)));
    assert!(usable("one two three four five six seven eight nine ten"));
}

#[test]
fn a_device_runs_one_trial_at_a_time() {
    let budget = TrialBudget::default();
    let lease = budget.start("device").unwrap();
    assert_eq!(budget.start("device").err(), Some(1));
    assert!(budget.start("other-device").is_ok());
    drop(lease);
    assert!(budget.start("device").is_ok());
}

#[test]
fn a_device_starts_at_most_six_trials_a_minute() {
    let budget = TrialBudget::default();
    for _ in 0..6 {
        drop(budget.start("device").unwrap());
    }
    let wait = budget.start("device").err().unwrap();
    assert!((1..=60).contains(&wait), "{wait}");
    assert!(budget.start("other-device").is_ok());
}
