use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::control::room::replay::MAX_REPLAY_RECORDS;
use crate::control::room::speech::MAX_UTTERANCES;
use crate::control::room::util::id;
use crate::control::room::{ConnectorPeer, Room};
use crate::storage::PrivateDir;

#[test]
fn replay_burst_reserves_live_speech_and_retires_terminal_records() {
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
    let (events, mut received) = mpsc::channel(128);
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
    room.receipt(&sid, "original", revision, "playing").unwrap();
    room.receipt(&sid, "original", revision, "playback_finished")
        .unwrap();
    let history_id = format!("{sid}:voice:original");
    for index in 0..MAX_REPLAY_RECORDS {
        room.replay_one(&sid, &history_id, &format!("replay-{index}"))
            .unwrap();
    }
    assert_eq!(
        room.replay_one(&sid, &history_id, "one-too-many")
            .unwrap_err()
            .status,
        429
    );
    let live = room.publish(
        &json!({"session_id":sid,"thread_id":"replay-thread","revision":revision,
            "utterance_id":"next-live","text":"Live reply after replay burst"}),
        false,
    );
    assert_eq!(live["status"], "queued");
    for _ in 0..MAX_REPLAY_RECORDS {
        let active = room.inner.lock().unwrap().browsers[&sid]
            .active
            .clone()
            .unwrap();
        assert!(active.starts_with("replay-"), "{active}");
        room.receipt(&sid, &active, revision, "playing").unwrap();
        room.receipt(&sid, &active, revision, "playback_finished")
            .unwrap();
    }
    assert_eq!(
        room.inner.lock().unwrap().browsers[&sid].active.as_deref(),
        Some("next-live")
    );
    room.receipt(&sid, "next-live", revision, "playing")
        .unwrap();
    room.receipt(&sid, "next-live", revision, "playback_finished")
        .unwrap();
    while received.try_recv().is_ok() {}
    for index in 0..MAX_UTTERANCES + 1 {
        let uid = format!("again-{index}");
        room.replay_one(&sid, &history_id, &uid).unwrap();
        room.receipt(&sid, &uid, revision, "playing").unwrap();
        room.receipt(&sid, &uid, revision, "playback_finished")
            .unwrap();
        while received.try_recv().is_ok() {}
    }
    let inner = room.inner.lock().unwrap();
    assert!(inner
        .utterances
        .values()
        .all(|record| record.replay_of.is_none()));
    assert!(inner.utterances.contains_key("original"));
    assert_eq!(
        inner
            .rows
            .iter()
            .find(|row| row.id == history_id)
            .unwrap()
            .status,
        "playback_finished"
    );
}

#[test]
fn replay_cancellation_close_and_leave_keep_original_history() {
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
    let (events, _received) = mpsc::channel(128);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "replay-thread").unwrap();
    let revision = room.snapshot(Some(&sid))["room"]["revision"]
        .as_u64()
        .unwrap();
    assert_eq!(
        room.publish(
            &json!({"session_id":sid,"thread_id":"replay-thread","revision":revision,
                "utterance_id":"original","text":"Original reply"}),
            false,
        )["status"],
        "queued"
    );
    room.receipt(&sid, "original", revision, "playing").unwrap();
    room.receipt(&sid, "original", revision, "playback_finished")
        .unwrap();
    let history_id = format!("{sid}:voice:original");
    room.replay_one(&sid, &history_id, "cancelled-replay")
        .unwrap();
    room.receipt(&sid, "cancelled-replay", revision, "cancelled_playing")
        .unwrap();
    assert!(!room
        .inner
        .lock()
        .unwrap()
        .utterances
        .contains_key("cancelled-replay"));
    room.replay_one(&sid, &history_id, "held-replay").unwrap();
    let turn = room.begin_turn(&sid).unwrap();
    assert_eq!(
        room.inner
            .lock()
            .unwrap()
            .rows
            .iter()
            .find(|row| row.id == history_id)
            .unwrap()
            .status,
        "playback_finished"
    );
    room.finish_turn(&sid, turn.revision);
    room.receipt(&sid, "held-replay", turn.revision, "playing")
        .unwrap();
    room.receipt(&sid, "held-replay", turn.revision, "playback_finished")
        .unwrap();
    let (other_events, _other_received) = mpsc::channel(8);
    let other = room
        .join("other-device".into(), "en".into(), other_events)
        .unwrap();
    room.select(&other, "replay-thread").unwrap();
    room.replay_one(&other, &history_id, "leaving-replay")
        .unwrap();
    room.leave(&other);
    assert!(!room
        .inner
        .lock()
        .unwrap()
        .utterances
        .contains_key("leaving-replay"));
    room.replay_one(&sid, &history_id, "closed-replay").unwrap();
    room.close_channel("replay-thread").unwrap();
    assert!(!room
        .inner
        .lock()
        .unwrap()
        .utterances
        .contains_key("closed-replay"));
    assert_eq!(
        room.inner
            .lock()
            .unwrap()
            .rows
            .iter()
            .find(|row| row.id == history_id)
            .unwrap()
            .status,
        "playback_finished"
    );
    room.leave(&sid);
    assert!(room
        .inner
        .lock()
        .unwrap()
        .utterances
        .contains_key("original"));
}
