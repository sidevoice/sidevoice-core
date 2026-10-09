use std::time::Duration;

use serde_json::{json, Value};

use super::{client_msg_id, resume_window, Outbound, ResumableCalls, SeenMessages, RING_FRAMES};

fn seq(text: &str) -> u64 {
    serde_json::from_str::<Value>(text).unwrap()["seq"]
        .as_u64()
        .unwrap()
}

#[test]
fn frames_are_numbered_and_a_returning_page_gets_exactly_what_it_missed() {
    let mut outbound = Outbound::default();
    for n in 0..5 {
        let text = outbound.stamp(json!({"type":"voice-ping","data":{"n":n}}));
        assert_eq!(seq(&text), n + 1);
    }
    let missed: Vec<u64> = outbound
        .since(2)
        .unwrap()
        .iter()
        .map(|(_, _, text)| seq(text))
        .collect();
    assert_eq!(missed, [3, 4, 5]);
    assert_eq!(outbound.since(5).unwrap().len(), 0, "nothing missed");
    assert_eq!(
        outbound.since(0).unwrap().len(),
        5,
        "a page that saw nothing"
    );
}

#[test]
fn each_kept_frame_says_what_it_is_and_whose_speech_it_carries() {
    let mut outbound = Outbound::default();
    outbound.stamp(json!({"type":"voice-speech","data":{"utterance_id":"u1"}}));
    outbound.stamp(json!({"type":"voice-ack","data":{"client_msg_id":"m"}}));
    let kept = outbound.since(0).unwrap();
    assert_eq!(
        (kept[0].0.as_str(), kept[0].1.as_deref()),
        ("voice-speech", Some("u1"))
    );
    assert_eq!(
        (kept[1].0.as_str(), kept[1].1.as_deref()),
        ("voice-ack", None)
    );
}

#[test]
fn a_page_that_missed_more_than_the_ring_holds_cannot_resume() {
    let mut outbound = Outbound::default();
    for _ in 0..RING_FRAMES + 10 {
        outbound.stamp(json!({"type":"voice-ping","data":{}}));
    }
    assert!(outbound.since(5).is_none(), "frames 6..=10 are gone");
    let kept = outbound.since(10).unwrap();
    assert_eq!(kept.len(), RING_FRAMES);
    assert_eq!(seq(&kept[0].2), 11);
}

#[test]
fn the_ring_is_bounded_by_bytes_too() {
    let mut outbound = Outbound::default();
    let big = "a".repeat(4 * 1024 * 1024);
    for _ in 0..6 {
        outbound.stamp(json!({"type":"voice-speech-audio","data":{"audio_base64":big}}));
    }
    assert!(outbound.bytes <= super::RING_BYTES);
    assert!(outbound.since(0).is_none());
    assert!(outbound.since(3).is_some());
}

#[tokio::test]
async fn a_token_takes_its_own_call_once_and_only_for_its_device() {
    let calls = ResumableCalls::default();
    let (attach, _parked) = tokio::sync::mpsc::channel(1);
    calls.open("s", "device", "token", attach);
    assert!(calls.take("s", "other", "token").is_none());
    assert!(calls.take("s", "device", "wrong").is_none());
    assert!(calls.take("s", "device", "").is_none());
    assert!(calls.take("s", "device", "token").is_some());
    assert!(calls.take("s", "device", "token").is_none(), "spent");
    calls.renew("s", "next");
    assert!(calls.take("s", "device", "next").is_some());
    calls.close("s");
    calls.renew("s", "again");
    assert!(calls.take("s", "device", "again").is_none(), "closed");
}

#[test]
fn a_repeated_message_gets_its_first_answer_within_a_bounded_memory() {
    let seen = SeenMessages::default();
    assert!(seen.answer("device", "m1").is_none());
    seen.remember("device", "m1", json!({"status":"playing"}));
    seen.remember("device", "m1", json!({"status":"other"}));
    assert_eq!(
        seen.answer("device", "m1"),
        Some(json!({"status":"playing"}))
    );
    assert!(seen.answer("another", "m1").is_none(), "ids are per device");
    for n in 0..super::SEEN_PER_DEVICE {
        seen.remember("device", &format!("x{n}"), Value::Null);
    }
    assert!(
        seen.answer("device", "m1").is_none(),
        "the oldest is forgotten"
    );
    assert!(seen.answer("device", "x1").is_some());
}

#[test]
fn ids_and_windows_are_read_defensively() {
    assert_eq!(client_msg_id(&json!({"client_msg_id":"abc"})), Some("abc"));
    assert_eq!(client_msg_id(&json!({"client_msg_id":""})), None);
    assert_eq!(
        client_msg_id(&json!({"client_msg_id":"x".repeat(65)})),
        None
    );
    assert_eq!(client_msg_id(&json!({"client_msg_id":5})), None);
    assert_eq!(resume_window(None), Duration::from_secs(60));
    assert_eq!(resume_window(Some("2.5")), Duration::from_millis(2500));
    assert_eq!(resume_window(Some("0")), Duration::ZERO);
    assert_eq!(resume_window(Some("-1")), Duration::from_secs(60));
    assert_eq!(resume_window(Some("soon")), Duration::from_secs(60));
}
