use serde_json::json;
use tokio::sync::{mpsc, watch};

use crate::control::room::util::id;
use crate::control::room::{ConnectorPeer, Room};
use crate::storage::PrivateDir;

#[test]
fn completed_voice_turn_uses_captured_focus_and_shared_outbox() {
    let directory = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
    let (requests, _receiver) = mpsc::channel(4);
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
        &json!({"thread":"old-thread","harness":"codex"}),
    )
    .unwrap();
    room.register(
        "connector",
        &json!({"thread":"new-thread","harness":"codex"}),
    )
    .unwrap();
    let (events, _received) = mpsc::channel(8);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "old-thread").unwrap();
    let turn = room.begin_turn(&sid).unwrap();
    assert_eq!(turn.thread_id.as_deref(), Some("old-thread"));
    room.select(&sid, "new-thread").unwrap();
    let accepted = room
        .queue_voice_input(&turn, "Words for the old thread")
        .unwrap();
    assert_eq!(accepted["accepted"], true);
    assert_eq!(accepted["id"], format!("{sid}:user-turn:{}", turn.revision));
    let rows = room.history(Some("old-thread"));
    assert_eq!(rows["messages"][0]["text"], "Words for the old thread");
    assert_eq!(rows["messages"][0]["status"], "pending");
    assert_eq!(
        room.history(Some("new-thread"))["messages"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let delivery = room.pending_delivery();
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0].3["thread"], "old-thread");
    assert_eq!(
        room.queue_voice_input(&turn, "Words for the old thread")
            .unwrap(),
        accepted
    );
}
