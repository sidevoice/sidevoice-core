//! Who may hear a reply, as a policy table. The core keeps the policy inside `Room::publish`, so
//! each row is set up through the room itself.

use serde_json::{json, Value};
use sidevoice_core::control::room::{ConnectorPeer, PeerRequest, Room};
use sidevoice_core::storage::PrivateDir;
use tokio::sync::{mpsc, watch};

struct Fixture {
    _root: tempfile::TempDir,
    room: Room,
    _requests: mpsc::Receiver<PeerRequest>,
    _stop: watch::Receiver<bool>,
    _events: Vec<mpsc::Receiver<Value>>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let room = Room::load(PrivateDir::open(root.path().join("core")).unwrap()).unwrap();
        let (sender, requests) = mpsc::channel(64);
        let (stop, stopped) = watch::channel(false);
        room.attach(
            "connector",
            ConnectorPeer {
                generation: "one".into(),
                sender,
                stop,
            },
        );
        for thread in ["thread-a", "thread-b"] {
            room.register("connector", &json!({"thread": thread, "harness": "codex"}))
                .unwrap();
        }
        Self {
            _root: root,
            room,
            _requests: requests,
            _stop: stopped,
            _events: Vec::new(),
        }
    }

    /// A browser in the call, listening to `thread`; returns its session and revision.
    fn listener(&mut self, thread: &str) -> (String, u64) {
        let (events, received) = mpsc::channel(256);
        self._events.push(received);
        let sid = self
            .room
            .join("device".into(), "en".into(), events)
            .unwrap();
        self.room.select(&sid, thread).unwrap();
        let revision = self.revision(&sid);
        (sid, revision)
    }

    fn revision(&self, sid: &str) -> u64 {
        self.room.snapshot(Some(sid))["room"]["revision"]
            .as_u64()
            .unwrap()
    }

    fn publish(&self, sid: &str, thread: &str, revision: u64) -> Value {
        self.room.publish(
            &json!({"session_id": sid, "thread_id": thread, "revision": revision,
                "utterance_id": uuid::Uuid::new_v4().to_string(), "text": "A reply"}),
            false,
        )
    }
}

fn text_only(answer: &Value, reason: &str) {
    assert_eq!(answer["status"], "text_only", "{answer}");
    assert_eq!(answer["reason"], reason, "{answer}");
    assert_eq!(answer["text_saved"], true, "the text is kept either way");
}

#[test]
fn the_current_quiet_listener_can_speak() {
    let mut fixture = Fixture::new();
    let (sid, revision) = fixture.listener("thread-a");
    let answer = fixture.publish(&sid, "thread-a", revision);
    assert_eq!(answer["status"], "queued", "{answer}");
    assert_eq!(
        (&answer["session_id"], &answer["revision"]),
        (&json!(sid), &json!(revision))
    );
}

#[test]
fn a_missing_session_precedes_other_denials() {
    let mut fixture = Fixture::new();
    fixture.listener("thread-b");
    text_only(&fixture.publish("nobody", "thread-a", 1), "session_changed");
}

#[test]
fn a_closed_call_is_not_saved_for_later_audio() {
    let mut fixture = Fixture::new();
    let (sid, revision) = fixture.listener("thread-a");
    fixture.room.leave(&sid);
    text_only(&fixture.publish(&sid, "thread-a", revision), "call_ended");
}

#[test]
fn a_wrong_focus_cannot_speak() {
    let mut fixture = Fixture::new();
    let (sid, revision) = fixture.listener("thread-a");
    text_only(
        &fixture.publish(&sid, "thread-b", revision),
        "focus_changed",
    );
}

#[test]
fn newer_input_speaks_at_the_current_audio_revision() {
    let mut fixture = Fixture::new();
    let (sid, revision) = fixture.listener("thread-a");
    let turn = fixture.room.begin_turn(&sid).unwrap();
    fixture
        .room
        .finish_turn(&sid, turn.revision, None, &Value::Null)
        .unwrap();
    assert!(turn.revision > revision);
    let answer = fixture.publish(&sid, "thread-a", revision);
    assert_ne!(answer["status"], "text_only", "{answer}");
    assert_eq!(answer["revision"], turn.revision, "{answer}");
}

#[test]
fn a_changed_audio_epoch_without_new_input_is_a_focus_change() {
    let mut fixture = Fixture::new();
    let (sid, revision) = fixture.listener("thread-a");
    fixture.room.select(&sid, "thread-b").unwrap();
    fixture.room.select(&sid, "thread-a").unwrap();
    assert!(fixture.revision(&sid) > revision);
    text_only(
        &fixture.publish(&sid, "thread-a", revision),
        "focus_changed",
    );
}

#[test]
fn a_user_speaking_still_gets_the_reply_for_the_call_to_hold_or_drop() {
    let mut fixture = Fixture::new();
    let (sid, _) = fixture.listener("thread-a");
    let turn = fixture.room.begin_turn(&sid).unwrap();
    let answer = fixture.publish(&sid, "thread-a", turn.revision);
    assert_eq!(answer["status"], "queued", "{answer}");
    assert_eq!(answer["revision"], turn.revision);
}

#[test]
fn a_replacement_listener_inherits_a_disconnected_asker_s_reply() {
    let mut fixture = Fixture::new();
    let (asker, revision) = fixture.listener("thread-a");
    fixture.room.leave(&asker);
    let (successor, successor_revision) = fixture.listener("thread-a");
    let answer = fixture.publish(&asker, "thread-a", revision);
    assert_eq!(answer["status"], "queued", "{answer}");
    assert_eq!(
        (&answer["session_id"], &answer["revision"]),
        (&json!(successor), &json!(successor_revision))
    );
}

#[test]
fn a_live_asker_is_not_replaced_by_another_browser() {
    let mut fixture = Fixture::new();
    let (asker, revision) = fixture.listener("thread-b");
    fixture.listener("thread-a");
    let answer = fixture.publish(&asker, "thread-a", revision);
    text_only(&answer, "focus_changed");
    assert!(answer.get("session_id").is_none());
}
