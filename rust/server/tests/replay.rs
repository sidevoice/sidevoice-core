use std::sync::Arc;

use serde_json::{json, Map};
use tokio::sync::watch;
use uuid::Uuid;

use super::support::{app_state, private_dir};
use crate::control::room::{ConnectorPeer, Room};
use crate::server::PinnedReplay;

#[test]
fn replay_admission_during_teardown_leaves_no_audio_or_record() {
    let (_temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let (requests, _request_receiver) = tokio::sync::mpsc::channel(4);
    let (stop, _stopped) = watch::channel(false);
    room.attach(
        "connector",
        ConnectorPeer {
            generation: Uuid::new_v4().to_string(),
            sender: requests,
            stop,
        },
    );
    room.register(
        "connector",
        &json!({"thread":"replay-thread","harness":"codex"}),
    )
    .unwrap();
    let (events, _received) = tokio::sync::mpsc::channel(128);
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
    let state = app_state(&dir, room, "host");
    let uid = format!("{sid}:replay:{}", Uuid::new_v4());
    let mut pending = state.replay_audio.lock().unwrap();
    let (ready, started) = std::sync::mpsc::channel();
    let closing = state.clone();
    let session = sid.clone();
    let teardown = std::thread::spawn(move || {
        ready.send(()).unwrap();
        closing.retire_session_replays(&session);
    });
    started.recv().unwrap();
    pending.insert(
        uid.clone(),
        Arc::new(PinnedReplay {
            speech: Arc::new(crate::providers::CloudSpeech {
                audio: vec![1, 2, 3],
                mime_type: "audio/mpeg".into(),
                alignment: None,
                timings_ms: Map::new(),
            }),
            voice: crate::models::ResolvedVoice {
                place: "elevenlabs".into(),
                model: "eleven_v3".into(),
                voice: "fixturevoice".into(),
                language: "en".into(),
                speed: 1.0,
            },
        }),
    );
    state
        .room
        .replay_one(&sid, &format!("{sid}:voice:original"), &uid)
        .unwrap();
    drop(pending);
    teardown.join().unwrap();
    assert!(!state.room.has_replay(&uid));
    assert!(state.replay_audio.lock().unwrap().is_empty());
}
