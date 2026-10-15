//! Speech admission at the room boundary. These cases exercise the public Room API.
use serde_json::{json, Value};
use sidevoice_core::control::room::{
    ConnectorPeer, Continuation, PeerRequest, Room, RoomError, VoiceTurn,
};
use sidevoice_core::storage::PrivateDir;
use tokio::sync::{mpsc, watch};

struct Fixture {
    _root: tempfile::TempDir,
    room: Room,
    _requests: mpsc::Receiver<PeerRequest>,
    _stop: watch::Receiver<bool>,
    events: Vec<mpsc::Receiver<Value>>,
}

trait BeginAtFocus {
    fn begin_at_focus(&self, sid: &str, turn_id: &str) -> Result<VoiceTurn, RoomError>;
}

impl BeginAtFocus for Room {
    fn begin_at_focus(&self, sid: &str, turn_id: &str) -> Result<VoiceTurn, RoomError> {
        let snapshot = self.snapshot(Some(sid));
        let binding = &snapshot["binding"];
        self.begin_turn(
            sid,
            turn_id,
            binding["thread_id"].as_str().unwrap_or(""),
            binding["binding_id"].as_str().unwrap_or(""),
        )
    }
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
            room.register("connector", &json!({"thread":thread,"harness":"codex"}))
                .unwrap();
        }
        Self {
            _root: root,
            room,
            _requests: requests,
            _stop: stopped,
            events: Vec::new(),
        }
    }

    fn listener(&mut self, thread: &str) -> (String, u64) {
        let (sender, receiver) = mpsc::channel(256);
        let sid = self
            .room
            .join("device".into(), "en".into(), sender)
            .unwrap();
        self.room.select(&sid, thread).unwrap();
        self.events.push(receiver);
        (
            sid.clone(),
            self.room.snapshot(Some(&sid))["room"]["revision"]
                .as_u64()
                .unwrap(),
        )
    }

    fn publish(&self, uid: &str, sid: &str, thread: &str, revision: u64, epoch: u64) -> Value {
        self.room.publish(
            &json!({"session_id":sid,"thread_id":thread,"revision":revision,
            "epoch":epoch,"utterance_id":uid,"text":format!("Reply {uid}")}),
        )
    }
}

#[test]
fn current_epoch_replies_are_all_accepted_in_order() {
    let mut f = Fixture::new();
    let (sid, revision) = f.listener("thread-a");
    for uid in ["ack", "checkpoint", "result"] {
        assert_eq!(
            f.publish(uid, &sid, "thread-a", revision, 0)["status"],
            "queued"
        );
    }
    let ids: Vec<_> = std::iter::from_fn(|| f.events[0].try_recv().ok())
        .filter(|event| event["type"] == "voice-reply")
        .map(|event| event["data"]["utterance_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["ack", "checkpoint", "result"]);
}

#[test]
fn an_open_turn_refuses_immediately_and_retry_keeps_the_refusal() {
    let mut f = Fixture::new();
    let (sid, _) = f.listener("thread-a");
    let turn = f.room.begin_at_focus(&sid, "turn").unwrap();
    let first = f.publish("refused", &sid, "thread-a", turn.revision, turn.epoch);
    assert_eq!(first["status"], "rejected");
    assert_eq!(first["reason"], "user_speaking");
    f.room
        .finish_turn(&sid, "turn", None, &Value::Null, Continuation::Standalone)
        .unwrap();
    assert_eq!(
        f.publish("refused", &sid, "thread-a", turn.revision, turn.epoch),
        first
    );
    assert!(f.room.history(Some("thread-a"))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["role"] != "assistant"));
}

#[test]
fn a_spoken_boundary_stales_old_context_but_accepts_fresh_context() {
    let mut f = Fixture::new();
    let (sid, revision) = f.listener("thread-a");
    let turn = f.room.begin_at_focus(&sid, "turn").unwrap();
    f.room
        .finish_turn(
            &sid,
            "turn",
            Some("New context"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    let stale = f.publish("stale", &sid, "thread-a", revision, 0);
    assert_eq!(stale["status"], "text_only");
    assert!(stale.get("reason").is_none());
    assert_eq!(
        f.room.history(Some("thread-a"))["messages"][1]["audio_reason"],
        "newer_turn"
    );
    assert_eq!(
        f.publish("fresh", &sid, "thread-a", turn.revision, turn.epoch)["status"],
        "queued"
    );
}

#[test]
fn typed_context_does_not_change_epoch_or_queued_replies() {
    let mut f = Fixture::new();
    let (sid, revision) = f.listener("thread-a");
    let binding = f.room.select(&sid, "thread-a").unwrap()["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        f.publish("before", &sid, "thread-a", revision, 0)["status"],
        "queued"
    );
    let typed = f
        .room
        .send_text(
            "Typed aside",
            &sid,
            "thread-a",
            &binding,
            "00000000-0000-4000-8000-000000000001",
        )
        .unwrap();
    assert_eq!(typed["epoch"], 0);
    assert_eq!(
        f.publish("after", &sid, "thread-a", revision, 0)["status"],
        "queued"
    );
}

#[test]
fn focus_and_session_switches_cannot_rewrite_a_reply_epoch() {
    let mut f = Fixture::new();
    let (first, revision) = f.listener("thread-a");
    let (second, _) = f.listener("thread-a");
    f.room.select(&first, "thread-b").unwrap();
    assert_eq!(
        f.publish("same-epoch", &first, "thread-a", revision, 0)["status"],
        "queued"
    );
    let turn = f.room.begin_at_focus(&second, "turn").unwrap();
    f.room
        .finish_turn(
            &second,
            "turn",
            Some("Later"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    let stale = f.publish("stale", &first, "thread-a", revision, 0);
    assert_eq!(stale["status"], "text_only");
    assert!(stale.get("reason").is_none());
    assert_eq!(turn.epoch, 1);
}

#[test]
fn no_listener_is_history_only_and_future_context_is_refused() {
    let mut f = Fixture::new();
    let (sid, revision) = f.listener("thread-a");
    f.room
        .set_speech(&sid, false, Some("agents_muted"))
        .unwrap();
    let muted = f.publish("muted", &sid, "thread-a", revision, 0);
    assert_eq!(muted["status"], "text_only");
    assert!(muted.get("reason").is_none());
    assert_eq!(
        f.room.history(Some("thread-a"))["messages"][0]["audio_reason"],
        "agents_muted"
    );
    let future = f.publish("future", &sid, "thread-a", revision, 1);
    assert_eq!(future["status"], "rejected");
}

#[test]
fn core_issued_speak_first_context_works_but_invented_sessions_do_not() {
    let mut f = Fixture::new();
    let (listener, _) = f.listener("thread-a");
    let registered = f
        .room
        .register("connector", &json!({"thread":"thread-a","harness":"codex"}))
        .unwrap();
    let context = &registered["initial_reply_context"];
    assert_eq!(context["epoch"], 0);
    let sid = context["session_id"].as_str().unwrap();
    let answer = f
        .room
        .publish(&json!({"binding_id":registered["binding_id"],
        "thread_id":"thread-a","session_id":sid,"revision":0,"epoch":0,
        "utterance_id":"speak-first","text":"Hello"}));
    assert_eq!(answer["status"], "queued");
    let invented = f
        .room
        .publish(&json!({"binding_id":registered["binding_id"],
        "thread_id":"thread-a","session_id":"typed:invented","revision":0,"epoch":0,
        "utterance_id":"invented","text":"Bypass"}));
    assert_eq!(invented["status"], "rejected");
    assert_eq!(invented["error"], "room.speech_context_invalid");
    assert!(!listener.is_empty());
}
