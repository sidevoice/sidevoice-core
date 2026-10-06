use super::*;

#[test]
fn defaults_and_configuration() {
    let default = Heartbeat::configured(None, None).unwrap();
    assert_eq!(default.interval, Duration::from_secs(15));
    assert_eq!(default.budget, Duration::from_secs(30));
    assert_eq!(Heartbeat::configured(Some("0"), Some("5")), None);
    assert_eq!(Heartbeat::configured(Some("never"), None), Some(default));
    assert_eq!(Heartbeat::configured(Some("-4"), None), Some(default));
    assert_eq!(Heartbeat::configured(None, Some("0")), Some(default));
    let custom = Heartbeat::configured(Some("2"), Some("3")).unwrap();
    assert_eq!(
        (custom.interval, custom.budget),
        (Duration::from_secs(2), Duration::from_secs(6))
    );
    let floored = Heartbeat::configured(Some("0.001"), Some("0.5")).unwrap();
    assert_eq!(
        (floored.interval, floored.budget),
        (Duration::from_millis(10), Duration::from_millis(10))
    );
}

#[test]
fn a_quiet_browser_is_asked_then_dropped() {
    let beat = Heartbeat::configured(Some("15"), Some("2")).unwrap();
    assert_eq!(beat.check(Duration::from_secs(3)), Beat::Quiet);
    assert_eq!(beat.check(Duration::from_secs(15)), Beat::Ask);
    assert_eq!(beat.check(Duration::from_secs(29)), Beat::Ask);
    assert_eq!(beat.check(Duration::from_secs(30)), Beat::Drop);
}
