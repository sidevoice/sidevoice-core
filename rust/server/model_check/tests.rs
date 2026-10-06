//! Unit tests of model-check admission: one run per check, remembered passes and rate limits.

use serde_json::{json, Map};

use super::budget::{check_key, Admission, CheckBudget};
use crate::types::SpeechStage;

fn started(admission: Admission) -> tokio::sync::watch::Receiver<Option<serde_json::Value>> {
    match admission {
        Admission::Start(receiver) => receiver,
        _ => panic!("expected the check to start"),
    }
}

#[test]
fn a_running_check_is_joined_and_a_passed_one_is_remembered() {
    let budget = CheckBudget::default();
    let first = started(budget.admit("check", "device", "openai"));
    let Admission::Join(joined) = budget.admit("check", "device", "openai") else {
        panic!("expected to join the running check");
    };
    budget.complete("check", json!({"ok":true,"step":"done"}));
    assert_eq!(*first.borrow(), Some(json!({"ok":true,"step":"done"})));
    assert_eq!(*joined.borrow(), Some(json!({"ok":true,"step":"done"})));
    let Admission::Cached(cached) = budget.admit("check", "device", "openai") else {
        panic!("expected the passed check to be remembered");
    };
    assert_eq!(cached, json!({"ok":true,"step":"done","remembered":true}));
}

#[test]
fn a_failed_check_is_not_remembered() {
    let budget = CheckBudget::default();
    started(budget.admit("check", "device", "openai"));
    budget.complete("check", json!({"ok":false,"step":"check"}));
    started(budget.admit("check", "device", "openai"));
}

#[test]
fn the_device_limit_is_six_checks_a_minute() {
    let budget = CheckBudget::default();
    for index in 0..6 {
        started(budget.admit(&format!("check-{index}"), "device", "openai"));
    }
    let Admission::Limited(wait, scope) = budget.admit("check-6", "device", "openai") else {
        panic!("expected the device limit");
    };
    assert_eq!(scope, "device");
    assert!((1..=60).contains(&wait), "{wait}");
    started(budget.admit("check-6", "other-device", "openai"));
}

#[test]
fn the_provider_limit_is_twelve_checks_a_minute_across_devices() {
    let budget = CheckBudget::default();
    for index in 0..12 {
        let device = format!("device-{}", index % 2);
        started(budget.admit(&format!("check-{index}"), &device, "openai"));
    }
    let Admission::Limited(_, scope) = budget.admit("check-12", "device-2", "openai") else {
        panic!("expected the provider limit");
    };
    assert_eq!(scope, "provider");
    started(budget.admit("check-12", "device-2", "elevenlabs"));
}

#[test]
fn the_check_key_changes_with_the_credential_without_containing_it() {
    let stage = SpeechStage {
        place: "openai".to_owned(),
        model: "gpt-4o-transcribe".to_owned(),
        options: Map::new(),
        build: None,
    };
    let key = check_key("stt", &stage, Some("es"), "sk-secret");
    assert!(!key.contains("sk-secret"));
    assert_eq!(key, check_key("stt", &stage, Some("es"), "sk-secret"));
    assert_ne!(key, check_key("stt", &stage, Some("es"), "sk-other"));
    assert_ne!(key, check_key("stt", &stage, Some("en"), "sk-secret"));
}

mod parity;
