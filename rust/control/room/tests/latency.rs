use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::control::room::latency_log::MAX_LATENCY_REPLIES;
use crate::control::room::{LatencyEvent, Room};
use crate::storage::PrivateDir;

#[test]
fn latency_input_keeps_only_recent_turns_and_seeds_recent_reply() {
    let directory = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
    let (events, _received) = mpsc::channel(4);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    for revision in 1..=MAX_LATENCY_REPLIES as u64 + 3 {
        room.latency_duration(&sid, "thread", revision, None, "audio_ms", revision as f64);
    }
    let mut inner = room.inner.lock().unwrap();
    assert_eq!(inner.latency.input_turns(), MAX_LATENCY_REPLIES);
    assert!(!inner.latency.has_input(&sid, "thread", 1));
    let newest = MAX_LATENCY_REPLIES as u64 + 3;
    inner.register_latency_reply(&sid, "thread", newest, "reply", "queued");
    assert_eq!(
        inner.latency.records(&sid).1.last().unwrap().input_ms[0].milliseconds,
        newest as f64
    );
}

/// A reply handed to a call is marked as dispatched there: the room synthesises nothing, and the reply ending unplayed
/// leaves what the call measured as it was.
#[test]
fn a_reply_handed_to_a_call_is_marked_dispatched_not_synthesised() {
    let (_directory, room, _) = super::support::pull_room(&["connector"]);
    room.register("connector", &json!({"thread":"t","harness":"claude"}))
        .unwrap();
    let (events, _received) = mpsc::channel(8);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "t").unwrap();
    let revision = room.snapshot(Some(&sid))["room"]["revision"]
        .as_u64()
        .unwrap();
    room.publish(
        &json!({"session_id":sid,"thread_id":"t","revision":revision,"utterance_id":"r","text":"Done."}),
        false,
    );
    room.latency_browser(
        &sid,
        "r",
        &json!({"audio_received_to_playback_scheduled_ms": 40.0}),
    );
    room.playback(&sid, "r", "unplayed", None, None, &Value::Null)
        .unwrap();
    let (_, marks, replies) = room.latency_records(&sid, "device").unwrap();
    let events: Vec<_> = marks
        .iter()
        .filter(|mark| mark.utterance_id.as_deref() == Some("r"))
        .map(|mark| mark.event)
        .collect();
    assert_eq!(
        events,
        [LatencyEvent::ReplyReceived, LatencyEvent::ReplyDispatched]
    );
    assert_eq!(
        replies[0].browser_ms.len(),
        1,
        "the call's own measure stays"
    );
}
