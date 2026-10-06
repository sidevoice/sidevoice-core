use serde_json::json;

use super::{attributes, Telemetry};

#[test]
fn disabled_exporter_has_no_instance_or_task() {
    assert!(Telemetry::configured(None).is_none());
    assert!(Telemetry::configured(Some(" ")).is_none());
    assert!(Telemetry::configured(Some("file:///tmp/collector")).is_none());
}

#[test]
fn private_values_and_unlisted_attributes_never_pass() {
    let kept = attributes(
        &json!({"sidevoice.thread_id":"x".repeat(500),"sidevoice.duration_ms":23.5,
        "sidevoice.transcript":"private", "sidevoice.audio":"private", "authorization":"secret",
        "sidevoice.reason":null}),
    );
    assert_eq!(kept["sidevoice.thread_id"].as_str().unwrap().len(), 200);
    assert_eq!(kept["sidevoice.duration_ms"], 23.5);
    assert!(!kept.to_string().contains("private"));
    assert!(!kept.to_string().contains("secret"));
}
