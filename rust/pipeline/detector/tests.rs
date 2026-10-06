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
