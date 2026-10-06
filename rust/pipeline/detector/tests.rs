use super::*;

#[tokio::test]
async fn playback_bar_uses_rustvani_threshold_and_restores_settings() {
    let mut settings = crate::models::default_settings(None, None);
    settings.turn_end_mode = "timer".into();
    let (detector, _events) = CallDetector::start(&settings).unwrap();
    assert_eq!(
        detector.active.lock().await.min_volume,
        settings.vad_min_volume
    );
    detector.listening_bar(true).await;
    let active = detector.active.lock().await;
    assert!(active.playing);
    assert_eq!(active.min_volume, 0.8);
    drop(active);
    detector.listening_bar(false).await;
    let active = detector.active.lock().await;
    assert!(!active.playing);
    assert_eq!(active.min_volume, settings.vad_min_volume);
}

#[tokio::test]
async fn reset_replaces_the_detector_and_keeps_the_playback_bar() {
    let settings = crate::models::default_settings(None, None);
    let (detector, _events) = CallDetector::start(&settings).unwrap();
    detector.listening_bar(true).await;
    let before = detector.generation.load(Ordering::Acquire);
    detector.reset().await;
    assert_eq!(detector.generation.load(Ordering::Acquire), before + 1);
    let active = detector.active.lock().await;
    assert!(active.playing);
    assert_eq!(active.min_volume, 0.8);
}

#[test]
fn vad_stop_follows_the_mode_and_the_room_override() {
    let mut settings = crate::models::default_settings(None, None);
    assert_eq!(
        vad_stop_secs(&settings, None),
        settings.smart_turn_min_silence
    );
    assert_eq!(vad_stop_secs(&settings, Some(1.5)), 1.5);
    settings.turn_end_mode = "timer".into();
    assert_eq!(
        vad_stop_secs(&settings, None),
        0.2 + settings.user_speech_timeout
    );
    assert_eq!(
        vad_stop_secs(&settings, Some(0.5)),
        0.5 + settings.user_speech_timeout
    );
    assert_eq!(parse_vad_stop(Some(" 0.7 ")), Some(0.7));
    assert_eq!(parse_vad_stop(Some("0")), Some(0.0));
    for unusable in [
        None,
        Some(""),
        Some("soon"),
        Some("-1"),
        Some("NaN"),
        Some("inf"),
    ] {
        assert_eq!(parse_vad_stop(unusable), None);
    }
}
