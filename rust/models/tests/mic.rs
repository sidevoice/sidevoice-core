//! Turn patience presets for the room's microphone detector.

use serde_json::json;

use super::support::defaults;
use crate::models::mic_settings;

#[test]
fn patience_maps_only_the_existing_room_presets() {
    let defaults = defaults();
    let mut fast = defaults.clone();
    fast.turn_patience = "fast".to_owned();
    let (fast, problem) = mic_settings(&fast, None);
    assert!(problem.is_none());
    assert_eq!(
        (
            fast.smart_turn_min_silence,
            fast.smart_turn_max_silence,
            fast.user_speech_timeout,
            fast.merge_window_secs
        ),
        (0.6, 2.5, 2.0, 0.0)
    );

    let (calm, problem) = mic_settings(&defaults, Some(&json!({"turn_patience":"calm"})));
    assert!(problem.is_none());
    assert_eq!(
        (
            calm.smart_turn_min_silence,
            calm.smart_turn_max_silence,
            calm.user_speech_timeout,
            calm.merge_window_secs
        ),
        (1.3, 4.0, 3.5, 1.5)
    );

    let (fallback, problem) = mic_settings(&defaults, Some(&json!({"turn_patience":"patient"})));
    assert_eq!(fallback.merge_window_secs, defaults.merge_window_secs);
    assert_eq!(
        problem.as_deref(),
        Some("Unknown patience; the room's own is used: patient")
    );
}

#[test]
fn device_detector_tuning_never_reaches_the_call() {
    // A device that stored the old numbers, or sends its own, keeps none of them: only its patience
    // shapes the room's numbers (Python `mic_settings`, the 2026-09-20 regression).
    let defaults = defaults();
    let loaded = crate::models::settings_from(
        Some(&json!({
            "turn_patience": "calm",
            "turn_end_mode": "timer",
            "user_speech_timeout": 9.0,
            "smart_turn_min_silence": 0.2,
            "smart_turn_max_silence": 12.0,
            "vad_confidence": 0.2,
            "vad_min_volume": 0.1,
            "vad_start_secs": 0.1,
            "merge_window_secs": 4.0,
            "audio_grace_seconds": 3.0
        })),
        &defaults,
    );
    assert!(loaded.issue.is_none());
    let (mic, problem) = mic_settings(&loaded.settings, Some(&json!({"vad_min_volume": 0.1})));
    assert!(problem.is_none());
    let call = mic.applied_to(&loaded.settings);
    assert_eq!(call.turn_end_mode, "smart_turn");
    assert_eq!(
        (
            call.user_speech_timeout,
            call.smart_turn_min_silence,
            call.smart_turn_max_silence,
            call.merge_window_secs
        ),
        (3.5, 1.3, 4.0, 1.5)
    );
    assert_eq!(
        (
            call.vad_confidence,
            call.vad_min_volume,
            call.vad_start_secs
        ),
        (
            defaults.vad_confidence,
            defaults.vad_min_volume,
            defaults.vad_start_secs
        )
    );
    // What is the device's own stays: its patience, grace, stages.
    assert_eq!(call.turn_patience, "calm");
    assert_eq!(call.audio_grace_seconds, 3.0);
    assert_eq!(call.stt.model, loaded.settings.stt.model);

    // The hello's `mic.turn_patience` wins over the stored one, as in Python.
    let (fast, _) = mic_settings(&loaded.settings, Some(&json!({"turn_patience": "fast"})));
    assert_eq!(fast.applied_to(&loaded.settings).merge_window_secs, 0.0);
}
