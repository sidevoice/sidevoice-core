//! Replays the person asks for: sent again as text, retired once they end, never touching the original's history.
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use super::support::report;
use crate::control::room::util::id;
use crate::control::room::utterances::MAX_UTTERANCES;
use crate::control::room::{ConnectorPeer, Room};
use crate::storage::PrivateDir;

/// A room with a binding on `replay-thread`, a call on it, and the history id of a reply that call heard.
fn heard_reply() -> (
    tempfile::TempDir,
    Room,
    String,
    mpsc::Receiver<Value>,
    String,
) {
    let directory = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
    let (requests, _request_receiver) = mpsc::channel(4);
    let (stop, _stopped) = watch::channel(false);
    room.attach(
        "connector",
        ConnectorPeer {
            generation: id(),
            sender: requests,
            stop,
        },
    );
    room.register(
        "connector",
        &json!({"thread":"replay-thread","harness":"codex"}),
    )
    .unwrap();
    let (events, received) = mpsc::channel(4096);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "replay-thread").unwrap();
    let revision = room.snapshot(Some(&sid))["room"]["revision"]
        .as_u64()
        .unwrap();
    let original = room.publish(
        &json!({"session_id":sid,"thread_id":"replay-thread","revision":revision,
            "utterance_id":"original","text":"Original reply"}),
        false,
    );
    assert_eq!(original["status"], "queued");
    report(&room, &sid, "original", "playing");
    report(&room, &sid, "original", "heard");
    let history_id = format!("{sid}:voice:original");
    (directory, room, sid, received, history_id)
}

fn status(room: &Room, history_id: &str) -> String {
    room.inner
        .lock()
        .unwrap()
        .journal
        .find(history_id)
        .unwrap()
        .status
        .clone()
}

fn kept(room: &Room, uid: &str) -> bool {
    room.inner.lock().unwrap().utterances.contains(uid)
}

#[test]
fn replays_that_ended_are_retired_and_the_original_stays() {
    let (_directory, room, sid, mut received, history_id) = heard_reply();
    for index in 0..MAX_UTTERANCES + 1 {
        let uid = format!("again-{index}");
        room.replay_one(&sid, &history_id, &uid).unwrap();
        report(&room, &sid, &uid, "playing");
        report(&room, &sid, &uid, "heard");
        while received.try_recv().is_ok() {}
    }
    let inner = room.inner.lock().unwrap();
    assert_eq!(inner.utterances.replay_count(), 0);
    assert!(inner.utterances.contains("original"));
    drop(inner);
    assert_eq!(status(&room, &history_id), "playback_finished");
}

#[test]
fn replay_skips_close_and_leave_keep_original_history() {
    let (_directory, room, sid, _received, history_id) = heard_reply();
    room.replay_one(&sid, &history_id, "skipped-replay")
        .unwrap();
    room.playback(
        &sid,
        "skipped-replay",
        "interrupted",
        Some("user_skipped"),
        None,
        &Value::Null,
    )
    .unwrap();
    assert!(!kept(&room, "skipped-replay"));
    room.replay_one(&sid, &history_id, "turn-replay").unwrap();
    room.begin_turn(&sid).unwrap();
    assert_eq!(status(&room, &history_id), "playback_finished");
    let (other_events, _other_received) = mpsc::channel(8);
    let other = room
        .join("other-device".into(), "en".into(), other_events)
        .unwrap();
    room.select(&other, "replay-thread").unwrap();
    room.replay_one(&other, &history_id, "leaving-replay")
        .unwrap();
    room.leave(&other);
    assert!(!kept(&room, "leaving-replay"));
    room.replay_one(&sid, &history_id, "closed-replay").unwrap();
    room.close_channel("replay-thread").unwrap();
    assert!(!kept(&room, "closed-replay"));
    assert_eq!(status(&room, &history_id), "playback_finished");
    room.leave(&sid);
    assert!(kept(&room, "original"));
}
