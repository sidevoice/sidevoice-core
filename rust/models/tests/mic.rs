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
