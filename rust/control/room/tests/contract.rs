use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::pull_room;
use crate::control::room::journal::Row;
use crate::control::room::voice_contract::sentence_start;
use crate::control::room::ConnectorPeer;
use crate::control::room::{Continuation, Room};
use crate::storage::PrivateDir;
use tokio::sync::watch;

struct Case {
    _dir: tempfile::TempDir,
    room: Room,
    bid: String,
    initial: Value,
    instance: String,
    connector_generation: String,
    sid: String,
    focus: String,
    events: mpsc::Receiver<Value>,
}

impl Case {
    fn new() -> Self {
        Self::with_browser_capacity(128)
    }
    fn with_browser_capacity(capacity: usize) -> Self {
        let (dir, room, generations) = pull_room(&["connector", "foreign"]);
        let registered = room
            .register(
                "connector",
                &json!({"thread":"t","harness":"fixture","client_ref":"conversation-t",
            "input_mode":"pull","voice_contract":1}),
            )
            .unwrap();
        let bid = registered["binding_id"].as_str().unwrap().to_owned();
        let initial = registered["initial_reply_context"].clone();
        let instance = registered["voice_contract_instance"]
            .as_str()
            .unwrap()
            .to_owned();
        let (sender, events) = mpsc::channel(capacity);
        let sid = room.join("device".into(), "en".into(), sender).unwrap();
        let focus = room.select(&sid, "t").unwrap()["binding"]["binding_id"]
            .as_str()
            .unwrap()
            .to_owned();
        Self {
            _dir: dir,
            room,
            bid,
            initial,
            instance,
            connector_generation: generations[0].clone(),
            sid,
            focus,
            events,
        }
    }
    fn publish(&self, uid: &str, text: &str) -> Value {
        self.room.contract_publish(
            "connector",
            &json!({"voice_contract":1,"binding_id":self.bid,
            "voice_contract_instance":self.instance,
            "event_id":format!("e-{uid}"),"utterance_id":uid,"text":text}),
            false,
        )
    }
    fn frame(&mut self, kind: &str) -> Option<Value> {
        std::iter::from_fn(|| self.events.try_recv().ok()).find(|frame| frame["type"] == kind)
    }
    fn turn(&self, id: &str, snapshot: Value) {
        self.room
            .begin_turn_with_snapshot(&self.sid, id, "t", &self.focus, &snapshot)
            .unwrap();
    }
    fn finish(
        &self,
        id: &str,
        text: Option<&str>,
        snapshot: Value,
        not_played: Value,
        continuation: Continuation,
    ) {
        self.room
            .finish_turn_with_evidence(
                &self.sid,
                id,
                text,
                &Value::Null,
                continuation,
                &snapshot,
                &not_played,
                false,
            )
            .unwrap();
    }
    fn pull(&self) -> Vec<Value> {
        self.room
            .pull_input(
                "connector",
                &json!({"binding_id":self.bid,"operation":"get"}),
            )
            .unwrap()["messages"]
            .as_array()
            .unwrap()
            .clone()
    }
}

#[test]
fn admission_is_truthful_idempotent_and_owned() {
    let mut c = Case::new();
    let request = json!({"voice_contract":1,"binding_id":c.bid,"voice_contract_instance":c.instance,"event_id":"e-a",
        "utterance_id":"a","text":"Approve X?"});
    let result = c.room.contract_publish("connector", &request, false);
    assert_eq!(result["status"], "accepted");
    assert_eq!(result["playback"], "queued");
    assert!(result["attempt_id"].as_str().is_some());
    assert_eq!(
        c.room.contract_publish("connector", &request, false),
        result
    );
    assert_eq!(c.room.contract_publish("connector", &request, true), result);
    assert_eq!(
        c.room.contract_publish("foreign", &request, false)["reason"],
        "binding_inactive"
    );
    let mut changed = request.clone();
    changed["text"] = json!("Approve Y?");
    assert_eq!(
        c.room.contract_publish("connector", &changed, false)["reason"],
        "identity_conflict"
    );
    let frame = c.frame("voice-reply").unwrap();
    assert_eq!(frame["data"]["utterance_id"], "a");
    assert_eq!(frame["data"]["attempt_id"], result["attempt_id"]);
    assert!(c.frame("voice-reply").is_none());
    let lookup = c.room.contract_get_reply(
        "connector",
        &json!({"binding_id":c.bid,"voice_contract_instance":c.instance,"utterance_id":"a"}),
    );
    assert_eq!(lookup["status"], "available");
    assert_eq!(lookup["audience_hearing"]["uncertain"], true);
    assert_eq!(
        c.room.contract_get_reply(
            "foreign",
            &json!({"binding_id":c.bid,"voice_contract_instance":c.instance,"utterance_id":"a"})
        )["status"],
        "unavailable"
    );
}

#[test]
fn lost_publish_ack_recovery_keeps_same_attempt_across_replacement_binding() {
    let mut c = Case::new();
    let request = json!({"voice_contract":1,"binding_id":c.bid,"voice_contract_instance":c.instance,
        "event_id":"lost-ack","utterance_id":"lost-original","text":"Once only"});
    let first = c.room.contract_publish("connector", &request, false);
    assert_eq!(first["status"], "accepted");
    assert_eq!(
        c.frame("voice-reply").unwrap()["data"]["attempt_id"],
        first["attempt_id"]
    );
    c.room.unregister("connector", &c.bid);
    let replacement = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"fixture",
        "client_ref":"conversation-t","voice_contract":1,"input_mode":"pull"}),
        )
        .unwrap();
    assert_ne!(replacement["binding_id"], c.bid);
    let mut recovery = request;
    recovery["recovery_binding_id"] = replacement["binding_id"].clone();
    let recovered = c.room.contract_publish("connector", &recovery, true);
    assert_eq!(recovered, first);
    assert!(c.frame("voice-reply").is_none());
}

#[test]
fn acoustic_start_cuts_off_later_pending_question_on_real_turn_path() {
    let mut c = Case::new();
    let a = c.publish("a", "Approve X?");
    c.frame("voice-reply").unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            a["attempt_id"].as_str().unwrap(),
            "heard",
            Some("complete"),
            Some(10),
            None,
        )
        .unwrap();
    let a_heard = json!({"utterance_id":"a","hearing":"complete","heard_chars":10});
    let snapshot =
        json!({"knowledge":"known","last_played":a_heard,"recent":[a_heard],"truncated":false});
    c.turn("spoken", snapshot.clone());
    let b = c.publish("b", "Approve Y?");
    assert_eq!(b["playback"], "pending");
    assert_eq!(
        c.frame("voice-reply-admitted").unwrap()["data"]["utterance_id"],
        "b"
    );
    assert!(c.frame("voice-reply").is_none());
    c.finish(
        "spoken",
        Some("Yes, go ahead."),
        snapshot,
        json!([{"utterance_id":"b","hearing":"not_played","heard_chars":0}]),
        Continuation::Standalone,
    );
    let words = c
        .pull()
        .into_iter()
        .find(|m| m["channel"] == "voice")
        .unwrap();
    assert_eq!(words["origin"], "spoken_room");
    assert_eq!(
        words["playback_at_turn_start"]["last_played"]["utterance_id"],
        "a"
    );
    assert_eq!(words["not_played_at_turn_start"][0]["utterance_id"], "b");
    assert_eq!(words["text"], "Yes, go ahead.");
    assert!(c.frame("voice-reply").is_none());
}

#[test]
fn overlapping_empty_turns_release_pending_once_after_all_close() {
    let mut c = Case::new();
    c.turn("one", Value::Null);
    c.turn("two", Value::Null);
    let result = c.publish("a", "Still relevant.");
    assert_eq!(result["playback"], "pending");
    c.frame("voice-reply-admitted").unwrap();
    c.finish("one", None, Value::Null, json!([]), Continuation::Merged);
    assert!(c.frame("voice-reply").is_none());
    c.finish(
        "two",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let reply = c.frame("voice-reply").unwrap();
    assert_eq!(reply["data"]["utterance_id"], "a");
    assert_eq!(reply["data"]["attempt_id"], result["attempt_id"]);
    assert_eq!(reply["data"]["epoch"], 2);
    assert!(c.frame("voice-reply").is_none());
}

#[test]
fn omitted_initial_context_resolves_current_epoch_after_empty_turn() {
    let mut c = Case::new();
    c.turn("empty-before-first-reply", Value::Null);
    c.finish(
        "empty-before-first-reply",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let admitted = c.publish("first-after-empty", "Current reply");
    assert_eq!(admitted["status"], "accepted", "{admitted}");
    let frame = c.frame("voice-reply").unwrap();
    assert_eq!(frame["data"]["epoch"], 1);
    let row_id = format!(
        "{}:voice:first-after-empty",
        c.initial["session_id"].as_str().unwrap()
    );
    let inner = c.room.inner.lock().unwrap();
    let row = inner.journal.find(&row_id).unwrap();
    assert_eq!(row.epoch, 1);
    assert_eq!(row.session, c.initial["session_id"]);
}

#[test]
fn empty_turn_auto_resume_uses_each_listeners_confirmed_sentence() {
    let mut c = Case::new();
    let (sender, mut other_events) = mpsc::channel(128);
    let other = c
        .room
        .join("other-device".into(), "en".into(), sender)
        .unwrap();
    c.room.select(&other, "t").unwrap();
    let text = "First sentence. Second sentence.";
    let first = c.publish("a", text);
    c.frame("voice-reply").unwrap();
    other_events.try_recv().unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            first["attempt_id"].as_str().unwrap(),
            "playing",
            Some("partial"),
            Some(20),
            None,
        )
        .unwrap();
    c.turn("empty", Value::Null);
    c.finish(
        "empty",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let mine = c.frame("voice-reply").unwrap();
    let theirs = std::iter::from_fn(|| other_events.try_recv().ok())
        .find(|event| event["type"] == "voice-reply")
        .unwrap();
    assert_ne!(mine["data"]["attempt_id"], first["attempt_id"]);
    assert_eq!(mine["data"]["replay_start_chars"], 16);
    assert_eq!(theirs["data"]["replay_start_chars"], 0);
    assert_eq!(mine["data"]["epoch"], 1);
}

#[test]
fn interrupted_replay_then_ack_and_final_resume_in_admission_order() {
    let mut c = Case::new();
    let original = c.publish("a", "Original");
    c.frame("voice-reply").unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            original["attempt_id"].as_str().unwrap(),
            "unplayed",
            Some("not_played"),
            Some(0),
            None,
        )
        .unwrap();
    c.room
        .send_text("Please repeat", &c.sid, "t", &c.focus, "repeat-context")
        .unwrap();
    let context = c
        .pull()
        .into_iter()
        .find(|m| m["text"] == "Please repeat")
        .unwrap();
    let fields = json!({"voice_contract":1,"voice_contract_instance":c.instance,
        "binding_id":c.bid,"session_id":context["session_id"],"revision":context["revision"],
        "epoch":context["epoch"],"context_message_id":context["message_id"]});
    let mut replay = fields.clone();
    replay["utterance_id"] = json!("a");
    replay["replay_id"] = json!("repeat-a");
    replay["mode"] = json!("restart");
    assert_eq!(
        c.room.contract_replay("connector", &replay)["status"],
        "accepted"
    );
    c.frame("voice-reply").unwrap();
    for (uid, text) in [("b", "Acknowledgement"), ("c", "Final answer")] {
        let mut publication = fields.clone();
        publication["event_id"] = json!(format!("event-{uid}"));
        publication["utterance_id"] = json!(uid);
        publication["text"] = json!(text);
        assert_eq!(
            c.room.contract_publish("connector", &publication, false)["status"],
            "accepted"
        );
        c.frame("voice-reply").unwrap();
    }
    c.turn("interruption", Value::Null);
    c.finish(
        "interruption",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let resumed: Vec<_> = std::iter::from_fn(|| c.events.try_recv().ok())
        .filter(|event| event["type"] == "voice-reply")
        .map(|event| event["data"]["utterance_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(resumed, ["a", "b", "c"]);
}

#[test]
fn abandoned_auto_resume_hands_off_one_correlated_recovery_note() {
    let mut c = Case::new();
    let original = c.publish("a", "First sentence. Second sentence.");
    c.frame("voice-reply").unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            original["attempt_id"].as_str().unwrap(),
            "playing",
            Some("partial"),
            Some(20),
            None,
        )
        .unwrap();
    c.turn("empty", Value::Null);
    c.room.unregister("connector", &c.bid);
    let replacement = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"fixture",
        "client_ref":"conversation-t","voice_contract":1,"input_mode":"pull"}),
        )
        .unwrap();
    c.finish(
        "empty",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let notes = c
        .room
        .pull_input(
            "connector",
            &json!({"binding_id":replacement["binding_id"],
        "operation":"get"}),
        )
        .unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let recovery: Vec<_> = notes
        .iter()
        .filter(|note| note["event"] == "recovery_needed")
        .collect();
    assert_eq!(recovery.len(), 1, "{notes:?}");
    assert_eq!(recovery[0]["hearing_changes"][0]["utterance_id"], "a");
    assert_eq!(recovery[0]["hearing_changes"][0]["heard_chars"], 20);
    assert_eq!(
        recovery[0]["playback_outcomes"].as_array().unwrap().len(),
        2
    );
    assert!(c.frame("voice-reply").is_none());
    c.room
        .contract_playback(
            &c.sid,
            "a",
            original["attempt_id"].as_str().unwrap(),
            "interrupted",
            Some("partial"),
            Some(25),
            Some("late_terminal"),
        )
        .unwrap();
    c.room
        .send_text("Later words", &c.sid, "t", &c.focus, "late-words")
        .unwrap();
    let later = c
        .room
        .pull_input(
            "connector",
            &json!({"binding_id":replacement["binding_id"],
        "operation":"get"}),
        )
        .unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let words = later
        .iter()
        .find(|message| message["text"] == "Later words")
        .unwrap();
    assert_eq!(words["hearing_changes"][0]["heard_chars"], 25);
    assert_eq!(
        later
            .iter()
            .filter(|message| message["event"] == "recovery_needed")
            .count(),
        1
    );
}

#[test]
fn physical_auto_resume_failure_hands_off_once_after_terminal_receipt() {
    let mut c = Case::new();
    let original = c.publish("a", "First sentence. Second sentence.");
    c.frame("voice-reply").unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            original["attempt_id"].as_str().unwrap(),
            "playing",
            Some("partial"),
            Some(20),
            None,
        )
        .unwrap();
    c.turn("empty", Value::Null);
    c.finish(
        "empty",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let resumed = c.frame("voice-reply").unwrap();
    let resumed_aid = resumed["data"]["attempt_id"].as_str().unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            resumed_aid,
            "failed",
            Some("unknown"),
            None,
            Some("synthesis_failed"),
        )
        .unwrap();
    let notes = c.pull();
    let recovery: Vec<_> = notes
        .iter()
        .filter(|item| item["event"] == "recovery_needed")
        .collect();
    assert_eq!(recovery.len(), 1, "{notes:?}");
    assert_eq!(recovery[0]["hearing_changes"][0]["utterance_id"], "a");
    assert_eq!(recovery[0]["hearing_changes"][0]["heard_chars"], 20);
    assert!(recovery[0]["playback_outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|outcome| outcome["reason"] == "synthesis_failed"));
    c.room
        .contract_playback(
            &c.sid,
            "a",
            resumed_aid,
            "failed",
            Some("unknown"),
            None,
            Some("duplicate_terminal"),
        )
        .unwrap();
    let after_ack = c
        .room
        .pull_input(
            "connector",
            &json!({"binding_id":c.bid,"operation":"get",
                "ack_ids":[recovery[0]["message_id"]]}),
        )
        .unwrap();
    assert!(after_ack["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["event"] != "recovery_needed"));
}

#[test]
fn multi_listener_auto_resume_handoff_distinguishes_complete_and_failed_targets() {
    let mut c = Case::new();
    let (sender, mut other_events) = mpsc::channel(128);
    let other = c
        .room
        .join("other-device".into(), "en".into(), sender)
        .unwrap();
    c.room.select(&other, "t").unwrap();
    let text = "First sentence. Second sentence.";
    let original = c.publish("a", text);
    c.frame("voice-reply").unwrap();
    other_events.try_recv().unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            original["attempt_id"].as_str().unwrap(),
            "playing",
            Some("partial"),
            Some(20),
            None,
        )
        .unwrap();
    c.turn("empty", Value::Null);
    c.finish(
        "empty",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let resumed = c.frame("voice-reply").unwrap();
    let resumed_aid = resumed["data"]["attempt_id"].as_str().unwrap();
    assert_eq!(
        std::iter::from_fn(|| other_events.try_recv().ok())
            .find(|event| event["type"] == "voice-reply")
            .unwrap()["data"]["attempt_id"],
        resumed_aid
    );
    c.room
        .contract_playback(
            &c.sid,
            "a",
            resumed_aid,
            "heard",
            Some("complete"),
            Some(text.chars().count() as u64),
            None,
        )
        .unwrap();
    assert!(c
        .pull()
        .iter()
        .all(|item| item["event"] != "recovery_needed"));
    c.room
        .contract_playback(
            &other,
            "a",
            resumed_aid,
            "failed",
            Some("unknown"),
            None,
            Some("device_failed"),
        )
        .unwrap();
    let notes = c.pull();
    let recovery: Vec<_> = notes
        .iter()
        .filter(|item| item["event"] == "recovery_needed")
        .collect();
    assert_eq!(recovery.len(), 1, "{notes:?}");
    assert_eq!(recovery[0]["session_id"], other);
    let facts = recovery[0]["hearing_changes"].as_array().unwrap();
    assert_eq!(facts.len(), 2);
    assert!(facts.iter().any(|fact| {
        fact["listener_session_id"] == c.sid
            && fact["hearing"] == "complete"
            && fact["heard_chars"] == text.chars().count() as u64
    }));
    assert!(facts.iter().any(|fact| {
        fact["listener_session_id"] == other
            && fact["hearing"] == "unknown"
            && fact["heard_chars"].is_null()
    }));
}

#[test]
fn fresh_replay_opens_second_recovery_handoff_episode_for_same_original() {
    let mut c = Case::new();
    c.publish("a", "Answer worth replaying");
    c.frame("voice-reply").unwrap();
    c.turn("first-empty", Value::Null);
    c.finish(
        "first-empty",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let first_auto = c.frame("voice-reply").unwrap();
    let first_aid = first_auto["data"]["attempt_id"].as_str().unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            first_aid,
            "failed",
            Some("unknown"),
            None,
            Some("device_failed"),
        )
        .unwrap();
    let first_note = c
        .pull()
        .into_iter()
        .find(|item| item["event"] == "recovery_needed")
        .unwrap();
    c.room
        .pull_input(
            "connector",
            &json!({"binding_id":c.bid,"operation":"get",
                "ack_ids":[first_note["message_id"]]}),
        )
        .unwrap();
    c.room
        .send_text("Replay now", &c.sid, "t", &c.focus, "replay-context")
        .unwrap();
    let context = c
        .pull()
        .into_iter()
        .find(|item| item["text"] == "Replay now")
        .unwrap();
    let replay = c.room.contract_replay(
        "connector",
        &json!({"voice_contract":1,"voice_contract_instance":c.instance,
            "binding_id":c.bid,"utterance_id":"a","replay_id":"second-episode",
            "mode":"restart","session_id":context["session_id"],
            "revision":context["revision"],"epoch":context["epoch"],
            "context_message_id":context["message_id"]}),
    );
    assert_eq!(replay["status"], "accepted", "{replay}");
    c.frame("voice-reply").unwrap();
    c.turn("second-empty", Value::Null);
    c.finish(
        "second-empty",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let second_auto = c.frame("voice-reply").unwrap();
    let second_aid = second_auto["data"]["attempt_id"].as_str().unwrap();
    assert_ne!(first_aid, second_aid);
    c.room
        .contract_playback(
            &c.sid,
            "a",
            second_aid,
            "failed",
            Some("unknown"),
            None,
            Some("device_failed_again"),
        )
        .unwrap();
    let second_note = c
        .pull()
        .into_iter()
        .find(|item| item["event"] == "recovery_needed")
        .unwrap();
    assert_ne!(first_note["message_id"], second_note["message_id"]);
    assert_eq!(second_note["hearing_changes"][0]["attempt_id"], second_aid);
    c.room
        .contract_playback(
            &c.sid,
            "a",
            second_aid,
            "failed",
            Some("unknown"),
            None,
            Some("duplicate_terminal"),
        )
        .unwrap();
    let after_ack = c
        .room
        .pull_input(
            "connector",
            &json!({"binding_id":c.bid,"operation":"get",
                "ack_ids":[second_note["message_id"]]}),
        )
        .unwrap();
    assert!(after_ack["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["event"] != "recovery_needed"));
}

#[test]
fn typed_input_fences_queued_without_spoken_epoch_or_acoustic_header() {
    let mut c = Case::new();
    let result = c.publish("a", "An answer");
    c.frame("voice-reply").unwrap();
    let before = c.room.snapshot(Some(&c.sid))["room"]["epoch"]
        .as_u64()
        .unwrap_or(0);
    c.room
        .send_text("Typed correction", &c.sid, "t", &c.focus, "typed-1")
        .unwrap();
    let after = c.room.snapshot(Some(&c.sid))["room"]["epoch"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(before, after);
    let words = c
        .pull()
        .into_iter()
        .find(|m| m["channel"] == "voice")
        .unwrap();
    assert_eq!(words["origin"], "typed_room");
    assert!(words.get("playback_at_turn_start").is_none());
    assert!(words.get("not_played_at_turn_start").is_none());
    assert!(words["playback_outcomes"].as_array().unwrap().is_empty());
    let fence = c.frame("voice-reply-withdrawn").unwrap();
    assert_eq!(fence["data"]["reason"], "new_words_pending_only");
    assert_eq!(fence["data"]["attempt_ids"][0], result["attempt_id"]);
    c.room
        .contract_playback(
            &c.sid,
            "a",
            result["attempt_id"].as_str().unwrap(),
            "unplayed",
            Some("not_played"),
            Some(0),
            Some("newer_turn"),
        )
        .unwrap();
    c.room
        .send_text("Next", &c.sid, "t", &c.focus, "typed-2")
        .unwrap();
    let next = c.pull().into_iter().find(|m| m["text"] == "Next").unwrap();
    assert_eq!(
        next["playback_outcomes"][0]["attempt_id"],
        result["attempt_id"]
    );
}

#[test]
fn typed_fence_does_not_claim_other_listeners_active_audio_stopped() {
    let mut c = Case::new();
    let (sender, mut other_events) = mpsc::channel(128);
    let other = c
        .room
        .join("other-device".into(), "en".into(), sender)
        .unwrap();
    c.room.select(&other, "t").unwrap();
    let first = c.publish("a", "An answer");
    c.frame("voice-reply").unwrap();
    other_events.try_recv().unwrap();
    c.room
        .send_text("Typed correction", &c.sid, "t", &c.focus, "typed-1")
        .unwrap();
    assert_eq!(
        c.frame("voice-reply-withdrawn").unwrap()["data"]["reason"],
        "new_words_pending_only"
    );
    let fence = other_events.try_recv().unwrap();
    assert_eq!(fence["type"], "voice-reply-withdrawn");
    assert_eq!(fence["data"]["attempt_ids"][0], first["attempt_id"]);
    let aid = first["attempt_id"].as_str().unwrap();
    c.room
        .contract_playback(&other, "a", aid, "playing", None, None, None)
        .unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            aid,
            "unplayed",
            Some("not_played"),
            Some(0),
            Some("newer_turn"),
        )
        .unwrap();
    c.room
        .contract_playback(&other, "a", aid, "heard", Some("complete"), Some(9), None)
        .unwrap();
    c.room
        .send_text("Next", &c.sid, "t", &c.focus, "typed-2")
        .unwrap();
    let next = c.pull().into_iter().find(|m| m["text"] == "Next").unwrap();
    assert_eq!(next["playback_outcomes"][0]["state"], "partial");
    let lookup = c.room.contract_get_reply(
        "connector",
        &json!({"binding_id":c.bid,
        "voice_contract_instance":c.instance,"utterance_id":"a"}),
    );
    assert_eq!(lookup["audience_hearing"]["confirmed_for_all_chars"], 0);
    assert_eq!(lookup["audience_hearing"]["uncertain"], false);
}

#[test]
fn repeated_progress_coalesces_per_listener_without_losing_latest_prefix() {
    let mut c = Case::new();
    let first = c.publish("a", "A twelve char answer");
    c.frame("voice-reply").unwrap();
    let aid = first["attempt_id"].as_str().unwrap();
    for prefix in 1..=12 {
        c.room
            .contract_playback(
                &c.sid,
                "a",
                aid,
                "playing",
                Some("partial"),
                Some(prefix),
                None,
            )
            .unwrap();
    }
    c.room
        .send_text("Current facts", &c.sid, "t", &c.focus, "typed-progress")
        .unwrap();
    let words = c
        .pull()
        .into_iter()
        .find(|m| m["text"] == "Current facts")
        .unwrap();
    let changes = words["hearing_changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["heard_chars"], 12);
    assert_eq!(changes[0]["listener_session_id"], c.sid);
}

#[test]
fn invalid_playback_status_cannot_change_hearing_attempt_or_replay_resume() {
    let mut c = Case::new();
    let text = "First sentence. Second sentence.";
    let published = c.publish("a", text);
    let offered = c.frame("voice-reply").unwrap();
    let aid = published["attempt_id"].as_str().unwrap();
    for (hearing, chars) in [("complete", text.chars().count() as u64), ("partial", 3)] {
        let rejected =
            c.room
                .contract_playback(&c.sid, "a", aid, "bogus", Some(hearing), Some(chars), None);
        assert_eq!(rejected.unwrap_err().key, "room.receipt_invalid");
    }
    assert_eq!(
        c.room
            .inner
            .lock()
            .unwrap()
            .voice_contract
            .attempt_state(aid),
        Some("queued")
    );
    assert_eq!(
        c.room
            .inner
            .lock()
            .unwrap()
            .journal
            .find(offered["data"]["history_id"].as_str().unwrap())
            .unwrap()
            .status,
        "queued"
    );
    let lookup = c.room.contract_get_reply(
        "connector",
        &json!({"binding_id":c.bid,"voice_contract_instance":c.instance,
            "utterance_id":"a"}),
    );
    assert_eq!(lookup["audience_hearing"]["complete_for_all"], false);
    assert_eq!(lookup["audience_hearing"]["confirmed_for_all_chars"], 0);
    assert_eq!(lookup["audience_hearing"]["uncertain"], true);
    c.room
        .send_text("Repeat", &c.sid, "t", &c.focus, "repeat-context")
        .unwrap();
    let words = c
        .pull()
        .into_iter()
        .find(|m| m["text"] == "Repeat")
        .unwrap();
    assert_eq!(words["hearing_changes"], json!([]));
    let replay = c.room.contract_replay(
        "connector",
        &json!({"voice_contract":1,"voice_contract_instance":c.instance,
            "binding_id":c.bid,"utterance_id":"a","replay_id":"retry-after-bogus",
            "mode":"resume","session_id":words["session_id"],
            "revision":words["revision"],"epoch":words["epoch"],
            "context_message_id":words["message_id"]}),
    );
    assert_eq!(replay["status"], "accepted", "{replay}");
    assert_eq!(
        c.frame("voice-reply").unwrap()["data"]["replay_start_chars"],
        0
    );
}

#[test]
fn binding_end_or_offline_withdraws_offered_unstarted_audio() {
    for offline in [false, true] {
        let mut c = Case::new();
        let published = c.publish("a", "Queued before binding loss");
        let offered = c.frame("voice-reply").unwrap();
        let aid = offered["data"]["attempt_id"].as_str().unwrap();
        assert_eq!(aid, published["attempt_id"]);
        if offline {
            c.room.detach("connector", &c.connector_generation);
        } else {
            c.room.unregister("connector", &c.bid);
        }
        let withdrawal = c.frame("voice-reply-withdrawn").unwrap();
        assert_eq!(withdrawal["data"]["attempt_ids"][0], aid);
        assert_eq!(
            withdrawal["data"]["reason"],
            "binding_inactive_pending_only"
        );
        assert_eq!(
            c.room
                .inner
                .lock()
                .unwrap()
                .voice_contract
                .attempt_state(aid),
            Some("fenced")
        );
        c.room
            .contract_playback(
                &c.sid,
                "a",
                aid,
                "unplayed",
                Some("not_played"),
                Some(0),
                Some("binding_inactive"),
            )
            .unwrap();
        let inner = c.room.inner.lock().unwrap();
        assert_eq!(inner.voice_contract.attempt_state(aid), Some("terminal"));
        assert_eq!(
            inner
                .journal
                .find(offered["data"]["history_id"].as_str().unwrap())
                .unwrap()
                .status,
            "unplayed"
        );
    }
}

#[test]
fn full_browser_queue_fails_closed_when_binding_withdrawal_cannot_be_delivered() {
    for offline in [false, true] {
        let c = Case::with_browser_capacity(1);
        let mut abort = c.room.browser_abort_watch(&c.sid).unwrap();
        let published = c.publish("a", "Queued before a full-channel fence");
        assert_eq!(published["status"], "accepted");
        // The sole channel slot holds the v1 offer. No receiver has read it yet.
        assert!(!*abort.borrow_and_update());
        if offline {
            c.room.detach("connector", &c.connector_generation);
        } else {
            c.room.unregister("connector", &c.bid);
        }
        assert!(*abort.borrow_and_update());
        assert_eq!(
            c.room
                .inner
                .lock()
                .unwrap()
                .voice_contract
                .attempt_state(published["attempt_id"].as_str().unwrap()),
            Some("fenced")
        );
        // The call loop prioritizes the abort watch over its ordinary output receiver. Its
        // close path retires the listener, even though that receiver still holds the offer.
        c.room.leave(&c.sid);
        assert_eq!(
            c.room
                .inner
                .lock()
                .unwrap()
                .voice_contract
                .attempt_state(published["attempt_id"].as_str().unwrap()),
            Some("terminal")
        );
    }
}

#[test]
fn binding_loss_preserves_playing_audio_and_late_physical_receipt() {
    let mut c = Case::new();
    let published = c.publish("a", "Already speaking");
    c.frame("voice-reply").unwrap();
    let aid = published["attempt_id"].as_str().unwrap();
    c.room
        .contract_playback(&c.sid, "a", aid, "playing", Some("partial"), Some(3), None)
        .unwrap();
    c.room.unregister("connector", &c.bid);
    assert_eq!(
        c.frame("voice-reply-withdrawn").unwrap()["data"]["reason"],
        "binding_inactive_pending_only"
    );
    assert_eq!(
        c.room
            .inner
            .lock()
            .unwrap()
            .voice_contract
            .attempt_state(aid),
        Some("playing")
    );
    c.room
        .contract_playback(
            &c.sid,
            "a",
            aid,
            "heard",
            Some("complete"),
            Some("Already speaking".chars().count() as u64),
            None,
        )
        .unwrap();
    assert_eq!(
        c.room
            .inner
            .lock()
            .unwrap()
            .voice_contract
            .attempt_state(aid),
        Some("terminal")
    );
}

#[test]
fn room_closed_binding_cannot_resurrect_from_cached_registration_or_old_ids() {
    let mut c = Case::new();
    let original = c.publish("old", "Old queued answer");
    assert_eq!(original["status"], "accepted");
    c.frame("voice-reply").unwrap();
    c.room.detach("connector", &c.connector_generation);
    let (closed, notification) = c.room.close_channel("t").unwrap();
    assert_eq!(closed["binding_id"], c.bid);
    assert!(notification.is_none(), "the connector was offline at close");
    assert!(c.room.binding_views().as_array().unwrap().is_empty());
    assert!(c.frame("voice-reply-withdrawn").is_some());
    let (sender, _requests) = mpsc::channel(4);
    let (stop, _stopped) = watch::channel(false);
    c.room.attach(
        "connector",
        ConnectorPeer {
            generation: "reconnected".into(),
            sender,
            stop,
        },
    );
    let registration = json!({"thread":"t","harness":"fixture",
        "client_ref":"conversation-t","input_mode":"pull","voice_contract":1,
        "binding_id":c.bid});
    assert_eq!(
        c.room.register("connector", &registration).unwrap(),
        json!({"status":"closed","reason":"binding_closed","binding_id":c.bid,
            "voice_contract":1})
    );
    assert!(c.room.binding_views().as_array().unwrap().is_empty());
    assert_eq!(
        c.publish("old", "Old queued answer")["reason"],
        "binding_inactive"
    );
    let old_scope = json!({"voice_contract":1,"binding_id":c.bid,
        "voice_contract_instance":c.instance,"utterance_id":"old"});
    assert_eq!(
        c.room.contract_get_reply("connector", &old_scope)["status"],
        "unavailable"
    );
    let mut fresh = registration;
    fresh.as_object_mut().unwrap().remove("binding_id");
    let joined = c.room.register("connector", &fresh).unwrap();
    assert_ne!(joined["binding_id"], c.bid);
    assert_eq!(joined["voice_contract"], 1);
    assert_eq!(c.room.binding_views().as_array().unwrap().len(), 1);
    let recovery = json!({"voice_contract":1,"binding_id":c.bid,
        "recovery_binding_id":joined["binding_id"],"voice_contract_instance":c.instance,
        "event_id":"e-old","utterance_id":"old","text":"Old queued answer"});
    assert_eq!(
        c.room.contract_publish("connector", &recovery, true)["reason"],
        "binding_inactive"
    );
    let lookup = json!({"voice_contract":1,"binding_id":c.bid,
        "recovery_binding_id":joined["binding_id"],"voice_contract_instance":c.instance,
        "utterance_id":"old"});
    assert_eq!(
        c.room.contract_get_reply("connector", &lookup)["status"],
        "unavailable"
    );
    let replay = json!({"voice_contract":1,"binding_id":c.bid,
        "recovery_binding_id":joined["binding_id"],"voice_contract_instance":c.instance,
        "replay_id":"old-replay","utterance_id":"old","mode":"resume"});
    assert_eq!(
        c.room.contract_replay("connector", &replay)["reason"],
        "binding_inactive"
    );
}

#[test]
fn transport_only_detach_renews_existing_binding_without_a_tombstone() {
    let c = Case::new();
    c.room.detach("connector", &c.connector_generation);
    let (sender, _requests) = mpsc::channel(4);
    let (stop, _stopped) = watch::channel(false);
    c.room.attach(
        "connector",
        ConnectorPeer {
            generation: "reconnected".into(),
            sender,
            stop,
        },
    );
    let joined = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"fixture",
        "client_ref":"conversation-t","voice_contract":1,"binding_id":c.bid}),
        )
        .unwrap();
    assert_eq!(joined["binding_id"], c.bid);
    assert_eq!(joined["voice_contract_instance"], c.instance);
    assert_eq!(c.room.binding_views().as_array().unwrap().len(), 1);
}

#[test]
fn failed_critical_withdrawal_retires_queued_audio_without_not_played_fact() {
    let c = Case::with_browser_capacity(1);
    let published = c.publish("a", "Queued but possibly sounding at socket loss");
    let aid = published["attempt_id"].as_str().unwrap();
    let abort = c.room.browser_abort_watch(&c.sid).unwrap();
    // The offer still fills the sole outbound slot when the binding goes away. Its
    // withdrawal cannot be queued, so the call must close rather than resume this seat.
    c.room.unregister("connector", &c.bid);
    assert!(*abort.borrow());
    c.room.leave(&c.sid);
    let mut payload = json!({});
    c.room
        .inner
        .lock()
        .unwrap()
        .contract_input_facts("t", &mut payload);
    assert_eq!(payload["hearing_changes"], json!([]));
    assert!(payload["playback_outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["attempt_id"] == aid
            && item["state"] == "failed"
            && item["reason"] == "focus_changed"));
}

#[test]
fn lost_playing_listener_preserves_confirmed_prefix() {
    let mut c = Case::new();
    let published = c.publish("a", "Already speaking");
    c.frame("voice-reply").unwrap();
    let aid = published["attempt_id"].as_str().unwrap();
    c.room
        .contract_playback(&c.sid, "a", aid, "playing", Some("partial"), Some(3), None)
        .unwrap();
    c.room.leave(&c.sid);
    let mut payload = json!({});
    c.room
        .inner
        .lock()
        .unwrap()
        .contract_input_facts("t", &mut payload);
    assert_eq!(payload["hearing_changes"][0]["heard_chars"], 3);
    assert_eq!(payload["hearing_changes"][0]["hearing"], "partial");
    assert!(payload["playback_outcomes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["attempt_id"] == aid && item["state"] == "partial"));
}

#[test]
fn context_rejects_partial_and_old_input_but_accepts_fresh_words() {
    let c = Case::new();
    let partial = json!({"voice_contract":1,"binding_id":c.bid,"voice_contract_instance":c.instance,"event_id":"e-p",
        "utterance_id":"p","text":"Partial","session_id":c.initial["session_id"]});
    assert_eq!(
        c.room.contract_publish("connector", &partial, false)["reason"],
        "invalid_request"
    );
    c.room
        .send_text("First", &c.sid, "t", &c.focus, "typed-1")
        .unwrap();
    let first = c
        .pull()
        .into_iter()
        .find(|m| m["channel"] == "voice")
        .unwrap();
    let mut fresh = json!({"voice_contract":1,"binding_id":c.bid,"voice_contract_instance":c.instance,"event_id":"e-f",
        "utterance_id":"f","text":"Fresh","session_id":first["session_id"],
        "revision":first["revision"],"epoch":first["epoch"],
        "context_message_id":first["message_id"]});
    assert_eq!(
        c.room.contract_publish("connector", &fresh, false)["status"],
        "accepted"
    );
    c.room
        .send_text("Second", &c.sid, "t", &c.focus, "typed-2")
        .unwrap();
    fresh["event_id"] = json!("e-stale");
    fresh["utterance_id"] = json!("stale");
    assert_eq!(
        c.room.contract_publish("connector", &fresh, false)["reason"],
        "context_stale"
    );
}

#[test]
fn evicted_input_context_never_reopens_startup_omission() {
    let c = Case::new();
    {
        let mut inner = c.room.inner.lock().unwrap();
        inner.append_row(Row {
            id: "old-input".into(),
            thread: "t".into(),
            role: "user",
            text: "prior words".into(),
            session: c.sid.clone(),
            status: "read".into(),
            payload: Some(json!({"message_id":"old-message"})),
            ..Row::default()
        });
        for n in 0..2001 {
            inner.append_row(Row {
                id: format!("settled-{n}"),
                thread: "other".into(),
                role: "assistant",
                status: "heard".into(),
                ..Row::default()
            });
        }
        assert!(inner.journal.latest_context("t").is_none());
        assert!(inner.journal.has_seen_input("t"));
    }
    let result = c.publish("after-eviction", "Do not reopen startup");
    assert_eq!(result["reason"], "context_stale");
}

#[test]
fn replay_uses_least_heard_listener_and_is_idempotent() {
    let mut c = Case::new();
    let (sender, mut other_events) = mpsc::channel(128);
    let other = c
        .room
        .join("other-device".into(), "en".into(), sender)
        .unwrap();
    c.room.select(&other, "t").unwrap();
    let text = "First sentence. Second sentence. Third sentence.";
    let original = c.publish("a", text);
    c.frame("voice-reply").unwrap();
    assert_eq!(other_events.try_recv().unwrap()["type"], "voice-reply");
    let aid = original["attempt_id"].as_str().unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            aid,
            "heard",
            Some("complete"),
            Some(text.chars().count() as u64),
            None,
        )
        .unwrap();
    c.room
        .contract_playback(
            &other,
            "a",
            aid,
            "unplayed",
            Some("not_played"),
            Some(0),
            None,
        )
        .unwrap();
    c.room
        .send_text("Again please", &c.sid, "t", &c.focus, "typed-replay")
        .unwrap();
    let context = c
        .pull()
        .into_iter()
        .find(|m| m["channel"] == "voice")
        .unwrap();
    let request = json!({"voice_contract":1,"voice_contract_instance":c.instance,
        "binding_id":c.bid,"utterance_id":"a","replay_id":"r-1","mode":"resume",
        "session_id":context["session_id"],"revision":context["revision"],"epoch":context["epoch"],
        "context_message_id":context["message_id"]});
    let result = c.room.contract_replay("connector", &request);
    assert_eq!(result["status"], "accepted");
    assert_eq!(result["playback"], "queued");
    assert_eq!(c.room.contract_replay("connector", &request), result);
    let frame = c.frame("voice-reply").unwrap();
    assert_eq!(frame["data"]["utterance_id"], "a");
    assert_eq!(frame["data"]["attempt_id"], result["attempt_id"]);
    assert_eq!(frame["data"]["replay_start_chars"], 0);
    let mut changed = request.clone();
    changed["mode"] = json!("restart");
    assert_eq!(
        c.room.contract_replay("connector", &changed)["reason"],
        "identity_conflict"
    );
}

#[test]
fn lost_replay_ack_recovery_keeps_same_attempt_across_replacement_binding() {
    let mut c = Case::new();
    let original = c.publish("a", "Repeat only once");
    c.frame("voice-reply").unwrap();
    c.room
        .contract_playback(
            &c.sid,
            "a",
            original["attempt_id"].as_str().unwrap(),
            "unplayed",
            Some("not_played"),
            Some(0),
            None,
        )
        .unwrap();
    c.room
        .send_text("Again", &c.sid, "t", &c.focus, "again-context")
        .unwrap();
    let context = c.pull().into_iter().find(|m| m["text"] == "Again").unwrap();
    let request = json!({"voice_contract":1,"voice_contract_instance":c.instance,
        "binding_id":c.bid,"utterance_id":"a","replay_id":"lost-replay-ack","mode":"restart",
        "session_id":context["session_id"],"revision":context["revision"],
        "epoch":context["epoch"],"context_message_id":context["message_id"]});
    let first = c.room.contract_replay("connector", &request);
    assert_eq!(first["status"], "accepted");
    c.frame("voice-reply").unwrap();
    c.room.unregister("connector", &c.bid);
    let replacement = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"fixture",
        "client_ref":"conversation-t","voice_contract":1,"input_mode":"pull"}),
        )
        .unwrap();
    let mut retry = request.clone();
    retry["recovery_binding_id"] = replacement["binding_id"].clone();
    assert_eq!(
        c.room.contract_replay("foreign", &retry)["reason"],
        "binding_inactive"
    );
    assert_eq!(c.room.contract_replay("connector", &retry), first);
    assert!(c.frame("voice-reply").is_none());
    let unrelated = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"fixture","client_ref":"other-conversation",
                "voice_contract":1,"input_mode":"pull"}),
        )
        .unwrap();
    let mut cross_owner = retry.clone();
    cross_owner["recovery_binding_id"] = unrelated["binding_id"].clone();
    assert_eq!(
        c.room.contract_get_reply("connector", &cross_owner)["status"],
        "unavailable"
    );
    assert_eq!(
        c.room.contract_replay("connector", &cross_owner)["reason"],
        "binding_inactive"
    );
    let current = replacement["binding_id"].as_str().unwrap();
    assert_eq!(
        c.room.contract_get_reply(
            "connector",
            &json!({"voice_contract":1,"voice_contract_instance":c.instance,
                "binding_id":c.bid,"recovery_binding_id":current,"utterance_id":"a"})
        )["status"],
        "available"
    );
    retry["replay_id"] = json!("new-under-dead-binding");
    assert_eq!(
        c.room.contract_replay("connector", &retry)["status"],
        "accepted"
    );
    let queued = c.frame("voice-reply").unwrap();
    assert_eq!(queued["data"]["utterance_id"], "a");
}

#[test]
fn focus_return_notes_available_text_only_original_and_replay_rechecks_focus() {
    let mut c = Case::new();
    c.room.select(&c.sid, "b").unwrap();
    let original = c.publish("a", "The contract is ready.");
    assert_eq!(original["playback"], "text_only");
    assert!(original["attempt_id"].is_null());
    c.focus = c.room.select(&c.sid, "t").unwrap()["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .into();
    let note = c
        .pull()
        .into_iter()
        .find(|m| m["event"] == "user_returned")
        .unwrap();
    assert_eq!(note["channel"], "note");
    assert_eq!(note["hearing_changes"][0]["utterance_id"], "a");
    assert_eq!(note["hearing_changes"][0]["hearing"], "not_played");
    assert!(note.get("origin").is_none());
    assert!(c.frame("voice-reply").is_none());
    let request = json!({"voice_contract":1,"voice_contract_instance":c.instance,
        "binding_id":c.bid,"utterance_id":"a","replay_id":"r-1","mode":"restart",
        "session_id":note["session_id"],"revision":note["revision"],"epoch":note["epoch"],
        "context_message_id":note["message_id"]});
    let replay = c.room.contract_replay("connector", &request);
    assert_eq!(replay["playback"], "queued");
    c.frame("voice-reply").unwrap();
    c.room.select(&c.sid, "b").unwrap();
    let withdrawn = c.frame("voice-reply-withdrawn").unwrap();
    assert_eq!(withdrawn["data"]["attempt_ids"][0], replay["attempt_id"]);
}

#[test]
fn park_and_return_emit_relevant_note_without_automatic_sound() {
    let mut c = Case::new();
    let first = c.publish("a", "Available on return");
    c.frame("voice-reply").unwrap();
    c.room.park(&c.sid, true);
    assert_eq!(
        c.frame("voice-reply-withdrawn").unwrap()["data"]["attempt_ids"][0],
        first["attempt_id"]
    );
    c.room.resume(&c.sid, &[]);
    let note = c
        .pull()
        .into_iter()
        .find(|m| m["event"] == "user_returned")
        .unwrap();
    assert_eq!(note["hearing_changes"][0]["utterance_id"], "a");
    assert_eq!(note["hearing_changes"][0]["hearing"], "unknown");
    assert!(c.frame("voice-reply").is_none());
}

#[test]
fn leaving_during_pending_turn_never_revives_automatic_audio() {
    let mut c = Case::new();
    c.turn("leaving", Value::Null);
    let first = c.publish("a", "Do not play after leave");
    assert_eq!(first["playback"], "pending");
    c.frame("voice-reply-admitted").unwrap();
    c.room.leave(&c.sid);
    assert!(c.frame("voice-reply").is_none());
    let (sender, mut next_events) = mpsc::channel(128);
    let next = c
        .room
        .join("new-device".into(), "en".into(), sender)
        .unwrap();
    c.room.select(&next, "t").unwrap();
    assert!(std::iter::from_fn(|| next_events.try_recv().ok())
        .all(|event| event["type"] != "voice-reply"));
}

#[test]
fn old_instance_cannot_retry_omitted_context_after_restart() {
    let (dir, room, _) = pull_room(&["connector"]);
    let old = room
        .register(
            "connector",
            &json!({"thread":"t","voice_contract":1,"client_ref":"conversation-t",
        "binding_id":"stable","input_mode":"pull"}),
        )
        .unwrap();
    let request = json!({"voice_contract":1,"voice_contract_instance":old["voice_contract_instance"],
        "binding_id":old["binding_id"],"event_id":"old-event","utterance_id":"old-utterance",
        "text":"Old text"});
    let first = room.contract_publish("connector", &request, false);
    assert_eq!(first["status"], "accepted", "{first}");
    drop(room);
    let new_room = Room::load(PrivateDir::open(dir.path().join("private")).unwrap()).unwrap();
    let (requests, _receiver) = mpsc::channel(4);
    let (stop, _stopped) = watch::channel(false);
    new_room.attach(
        "connector",
        ConnectorPeer {
            generation: "new".into(),
            sender: requests,
            stop,
        },
    );
    let new = new_room
        .register(
            "connector",
            &json!({"thread":"t","voice_contract":1,"client_ref":"conversation-t",
        "binding_id":"stable","input_mode":"pull"}),
        )
        .unwrap();
    assert_ne!(
        new["voice_contract_instance"],
        old["voice_contract_instance"]
    );
    assert_eq!(
        new_room.contract_publish("connector", &request, false)["reason"],
        "binding_inactive"
    );
    let mut current = request;
    current["binding_id"] = new["binding_id"].clone();
    current["voice_contract_instance"] = new["voice_contract_instance"].clone();
    current["event_id"] = json!("new-event");
    current["utterance_id"] = json!("new-utterance");
    assert_eq!(
        new_room.contract_publish("connector", &current, false)["status"],
        "accepted"
    );
}

#[test]
fn recovery_through_replacement_binding_requires_stored_identity_and_instance() {
    let (_dir, room, _) = pull_room(&["connector", "foreign"]);
    let old = room
        .register(
            "connector",
            &json!({"thread":"t","voice_contract":1,
        "client_ref":"conversation-t","binding_id":"stable","input_mode":"pull"}),
        )
        .unwrap();
    let old_bid = old["binding_id"].as_str().unwrap();
    room.unregister("connector", old_bid);
    let unrelated = room
        .register(
            "connector",
            &json!({"thread":"t","voice_contract":1,
        "client_ref":"other-conversation","input_mode":"pull"}),
        )
        .unwrap();
    let mut request = json!({"voice_contract":1,"voice_contract_instance":old["voice_contract_instance"],
        "binding_id":old_bid,"recovery_binding_id":unrelated["binding_id"],
        "event_id":"recovered-event","utterance_id":"recovered-original","text":"Recover me"});
    assert_eq!(
        room.contract_publish("connector", &request, true)["reason"],
        "binding_inactive"
    );
    let replacement = room
        .register(
            "connector",
            &json!({"thread":"t","voice_contract":1,
        "client_ref":"conversation-t","input_mode":"pull"}),
        )
        .unwrap();
    assert_eq!(
        replacement["voice_contract_instance"],
        old["voice_contract_instance"]
    );
    request["recovery_binding_id"] = replacement["binding_id"].clone();
    assert_eq!(
        room.contract_publish("foreign", &request, true)["reason"],
        "binding_inactive"
    );
    let recovered = room.contract_publish("connector", &request, true);
    assert_eq!(recovered["status"], "accepted", "{recovered}");
    assert_eq!(recovered["playback"], "text_only");
    assert!(recovered["attempt_id"].is_null());
    assert_eq!(
        room.contract_publish("connector", &request, true),
        recovered
    );
    let mut wrong_instance = request;
    wrong_instance["voice_contract_instance"] = json!("wrong-process");
    assert_eq!(
        room.contract_publish("connector", &wrong_instance, true)["reason"],
        "binding_inactive"
    );
}

#[test]
fn malformed_and_cross_conversation_snapshot_becomes_unknown() {
    let mut c = Case::new();
    let a = c.publish("a", "Present");
    c.frame("voice-reply").unwrap();
    let bogus = json!({"knowledge":"known","last_played":{
        "utterance_id":"foreign","hearing":"complete","heard_chars":7},
        "recent":[],"truncated":false});
    c.turn("spoken", bogus.clone());
    c.finish(
        "spoken",
        Some("Hi"),
        bogus,
        json!([{"utterance_id":"foreign","hearing":"not_played","heard_chars":0}]),
        Continuation::Standalone,
    );
    let words = c
        .pull()
        .into_iter()
        .find(|m| m["channel"] == "voice")
        .unwrap();
    assert_eq!(words["playback_at_turn_start"]["knowledge"], "unknown");
    assert_eq!(words["not_played_at_turn_start"], json!([]));
    assert!(a["attempt_id"].as_str().is_some());
}

#[test]
fn scalar_sentence_start_handles_unicode_decimals_abbreviations_and_quotes() {
    let text = "Dr. Vale paid 3.14. Next? “Olá!”\nFinal sentence.";
    let position = |needle: &str| text[..text.find(needle).unwrap()].chars().count() as u64;
    assert_eq!(sentence_start(text, position("Vale") + 2), 0);
    assert_eq!(sentence_start(text, position("Next") + 2), position("Next"));
    assert_eq!(sentence_start(text, position("Olá") + 2), position("“Olá"));
    assert_eq!(
        sentence_start(text, position("Final") + 2),
        position("Final")
    );
}

#[test]
fn idempotency_ceiling_rejects_new_ids_and_preserves_old_result() {
    let c = Case::new();
    c.room.select(&c.sid, "b").unwrap();
    let first = json!({"voice_contract":1,"voice_contract_instance":c.instance,
        "binding_id":c.bid,"event_id":"e-0","utterance_id":"u-0","text":"First"});
    let prior = c.room.contract_publish("connector", &first, false);
    assert_eq!(prior["status"], "accepted");
    for n in 1..256 {
        let result = c.publish(&format!("u-{n}"), "Later");
        assert_eq!(result["status"], "accepted", "{n}: {result}");
    }
    assert_eq!(c.publish("after-cap", "No more")["reason"], "capacity_full");
    assert_eq!(c.room.contract_publish("connector", &first, false), prior);
    let mut reused_original = first.clone();
    reused_original["event_id"] = json!("new-event-old-original");
    assert_eq!(
        c.room
            .contract_publish("connector", &reused_original, false)["reason"],
        "identity_conflict"
    );
    assert_eq!(
        c.room.contract_get_reply(
            "connector",
            &json!({"binding_id":c.bid,
        "voice_contract_instance":c.instance,"utterance_id":"u-0"})
        )["status"],
        "unavailable"
    );
}

#[test]
fn pending_capacity_rejects_ninth_without_evicting_first() {
    let mut c = Case::new();
    c.turn("open", Value::Null);
    let first = c.publish("p-0", "First pending");
    for n in 1..8 {
        assert_eq!(
            c.publish(&format!("p-{n}"), "Pending")["playback"],
            "pending"
        );
    }
    assert_eq!(c.publish("p-8", "Overflow")["reason"], "capacity_full");
    c.finish(
        "open",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let frames: Vec<Value> = std::iter::from_fn(|| c.events.try_recv().ok())
        .filter(|frame| frame["type"] == "voice-reply")
        .collect();
    assert_eq!(frames.len(), 8);
    assert_eq!(frames[0]["data"]["attempt_id"], first["attempt_id"]);
}

#[test]
fn original_cache_evicts_old_text_only_before_live_pending() {
    let mut c = Case::new();
    c.room.select(&c.sid, "b").unwrap();
    for n in 0..128 {
        assert_eq!(
            c.publish(&format!("cold-{n}"), "History")["playback"],
            "text_only"
        );
    }
    c.focus = c.room.select(&c.sid, "t").unwrap()["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let context = c
        .pull()
        .into_iter()
        .find(|message| message["event"] == "user_returned")
        .unwrap();
    c.turn("waiting", Value::Null);
    for n in 0..8 {
        let result = c.room.contract_publish(
            "connector",
            &json!({"voice_contract":1,"voice_contract_instance":c.instance,
                "binding_id":c.bid,"event_id":format!("e-pending-{n}"),
                "utterance_id":format!("pending-{n}"),"text":"Keep this pending",
                "session_id":context["session_id"],"revision":context["revision"],
                "epoch":context["epoch"],"context_message_id":context["message_id"]}),
            false,
        );
        assert_eq!(result["playback"], "pending", "{n}: {result}");
    }
    let lookup = |uid: &str| {
        c.room.contract_get_reply(
            "connector",
            &json!({"binding_id":c.bid,"voice_contract_instance":c.instance,
                "utterance_id":uid}),
        )
    };
    assert_eq!(lookup("cold-0")["status"], "unavailable");
    for n in 0..8 {
        assert_eq!(lookup(&format!("pending-{n}"))["status"], "available");
    }
    c.finish(
        "waiting",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    let sent: Vec<Value> = std::iter::from_fn(|| c.events.try_recv().ok())
        .filter(|frame| frame["type"] == "voice-reply")
        .collect();
    assert_eq!(sent.len(), 8);
}

#[test]
fn pending_deadline_and_original_ttl_fail_closed_without_replay() {
    let mut c = Case::new();
    c.turn("waiting", Value::Null);
    let admitted = c.publish("a", "Pending until deadline");
    assert_eq!(admitted["playback"], "pending");
    c.frame("voice-reply-admitted").unwrap();
    c.room
        .inner
        .lock()
        .unwrap()
        .voice_contract
        .force_deadline(admitted["attempt_id"].as_str().unwrap());
    c.finish(
        "waiting",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    assert!(c.frame("voice-reply").is_none());
    let row_id = format!("{}:voice:a", c.initial["session_id"].as_str().unwrap());
    assert_eq!(
        c.room
            .inner
            .lock()
            .unwrap()
            .journal
            .find(&row_id)
            .unwrap()
            .status,
        "unplayed"
    );
    c.room
        .send_text("After deadline", &c.sid, "t", &c.focus, "after-deadline")
        .unwrap();
    let words = c
        .pull()
        .into_iter()
        .find(|m| m["text"] == "After deadline")
        .unwrap();
    assert_eq!(words["playback_outcomes"][0]["reason"], "expired");
    c.room
        .inner
        .lock()
        .unwrap()
        .voice_contract
        .age_original("a");
    assert_eq!(
        c.room.contract_get_reply(
            "connector",
            &json!({"binding_id":c.bid,
        "voice_contract_instance":c.instance,"utterance_id":"a"})
        )["status"],
        "unavailable"
    );
    let replay = c.room.contract_replay(
        "connector",
        &json!({"binding_id":c.bid,
        "voice_contract_instance":c.instance,"utterance_id":"a","replay_id":"expired-replay",
        "mode":"restart","session_id":words["session_id"],"revision":words["revision"],
        "epoch":words["epoch"],"context_message_id":words["message_id"]}),
    );
    assert_eq!(replay["status"], "failed");
    assert_eq!(replay["reason"], "unavailable");
}

#[test]
fn expired_original_cannot_gain_fresh_auto_resume_deadline_on_empty_turn() {
    let mut c = Case::new();
    let published = c.publish("a", "Old answer");
    let offered = c.frame("voice-reply").unwrap();
    c.room
        .inner
        .lock()
        .unwrap()
        .voice_contract
        .age_original("a");
    c.turn("empty", Value::Null);
    c.finish(
        "empty",
        None,
        Value::Null,
        json!([]),
        Continuation::Standalone,
    );
    assert!(c.frame("voice-reply").is_none());
    let inner = c.room.inner.lock().unwrap();
    assert_eq!(
        inner
            .voice_contract
            .attempt_state(published["attempt_id"].as_str().unwrap()),
        Some("terminal")
    );
    assert_eq!(
        inner
            .journal
            .find(offered["data"]["history_id"].as_str().unwrap())
            .unwrap()
            .status,
        "unplayed"
    );
    drop(inner);
    assert_eq!(
        c.room.contract_get_reply(
            "connector",
            &json!({"binding_id":c.bid,"voice_contract_instance":c.instance,
                "utterance_id":"a"}),
        )["status"],
        "unavailable"
    );
    assert_eq!(c.publish("a", "Old answer"), published);
}

#[test]
fn delayed_queued_attempt_has_browser_deadline_and_pending_only_expiry_fence() {
    let mut c = Case::new();
    let first = c.publish("a", "Queued, but do not start late");
    let offered = c.frame("voice-reply").unwrap();
    assert_eq!(offered["data"]["attempt_id"], first["attempt_id"]);
    assert!(
        offered["data"]["auto_start_deadline_ms"].as_u64().unwrap()
            > crate::control::room::util::millis()
    );
    c.room
        .inner
        .lock()
        .unwrap()
        .voice_contract
        .force_deadline(first["attempt_id"].as_str().unwrap());
    assert_eq!(c.publish("b", "A fresh attempt")["status"], "accepted");
    let fence = c.frame("voice-reply-withdrawn").unwrap();
    assert_eq!(fence["data"]["attempt_ids"][0], first["attempt_id"]);
    assert_eq!(fence["data"]["reason"], "expired_pending_only");
    let row_id = format!("{}:voice:a", c.initial["session_id"].as_str().unwrap());
    assert_eq!(
        c.room
            .inner
            .lock()
            .unwrap()
            .journal
            .find(&row_id)
            .unwrap()
            .status,
        "unplayed"
    );
}
