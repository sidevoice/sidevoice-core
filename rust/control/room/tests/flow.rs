//! Task T core acceptance cases. Browser and native host.voice share these room frames.
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::pull_room;
use super::support::BeginAtFocus;
use crate::control::room::{Continuation, Room};

struct Case {
    _dir: tempfile::TempDir,
    room: Room,
    bid: String,
    sid: String,
    events: mpsc::Receiver<Value>,
}
impl Case {
    fn new(pull: bool) -> Self {
        let (dir, room, _) = pull_room(&["connector"]);
        let bid = room
            .register(
                "connector",
                &json!({"thread":"t","harness":"codex",
            "input_mode":if pull {"pull"} else {"push"}}),
            )
            .unwrap()["binding_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let (sender, events) = mpsc::channel(256);
        let sid = room.join("device".into(), "en".into(), sender).unwrap();
        room.select(&sid, "t").unwrap();
        Self {
            _dir: dir,
            room,
            bid,
            sid,
            events,
        }
    }
    fn revision(&self) -> u64 {
        self.room.snapshot(Some(&self.sid))["room"]["revision"]
            .as_u64()
            .unwrap()
    }
    fn publish(&self, uid: &str, epoch: u64, text: &str) -> Value {
        self.room.publish(
            &json!({"session_id":self.sid,"thread_id":"t","revision":self.revision(),
            "epoch":epoch,"utterance_id":uid,"text":text}),
        )
    }
    fn report(&self, uid: &str, status: &str, reason: Option<&str>, heard: Option<u64>) {
        self.room
            .playback(&self.sid, uid, status, reason, heard, &Value::Null)
            .unwrap();
    }
    fn hearing(&self, uid: &str, outcome: &str, heard_chars: Option<u64>) {
        self.room
            .reply_hearing(
                &self.sid,
                &format!("{}:voice:{uid}", self.sid),
                uid,
                outcome,
                heard_chars,
            )
            .unwrap();
    }
    fn say(&self, id: &str, text: Option<&str>) -> u64 {
        let turn = self.room.begin_at_focus(&self.sid, id).unwrap();
        self.room
            .finish_turn(&self.sid, id, text, &Value::Null, Continuation::Standalone)
            .unwrap();
        turn.epoch
    }
    fn input(&self, text: &str) -> Value {
        let inner = self.room.inner.lock().unwrap();
        inner
            .journal
            .find_input(text)
            .unwrap()
            .payload
            .clone()
            .unwrap()
    }
    fn notes(&self) -> Vec<Value> {
        let mut inner = self.room.inner.lock().unwrap();
        inner
            .journal
            .input_mut("t")
            .filter(|row| row.role == "note")
            .map(|row| row.payload.clone().unwrap())
            .collect()
    }
    fn frames(&mut self, kind: &str) -> Vec<Value> {
        std::iter::from_fn(|| self.events.try_recv().ok())
            .filter(|event| event["type"] == kind)
            .collect()
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
fn semantic_hearing_is_independent_of_audio_cause_and_missing_receipt() {
    let c = Case::new(true);
    c.publish("r", 0, "é🐙ab");
    c.room
        .playback_transport(
            &c.sid,
            "r",
            "interrupted",
            Some("agents_muted"),
            Some(2),
            &Value::Null,
        )
        .unwrap();
    let row = &c.room.history(Some("t"))["messages"][0];
    assert_eq!(row["audio_reason"], "agents_muted");
    assert_eq!(row["hearing"], Value::Null);
    assert_eq!(row["hearing_unconfirmed"], true);
    assert_eq!(row["heard_chars"], Value::Null);
    c.hearing("r", "partial", Some(2));
    let row = &c.room.history(Some("t"))["messages"][0];
    assert_eq!(row["hearing"], "partial");
    assert_eq!(row["heard_chars"], 2);
    assert_eq!(row["hearing_unconfirmed"], false);
}

#[test]
fn text_only_reply_can_be_acknowledged_as_read_or_not_heard() {
    for outcome in ["heard", "not_heard"] {
        let c = Case::new(true);
        c.room
            .set_speech(&c.sid, false, Some("agents_muted"))
            .unwrap();
        assert_eq!(c.publish("r", 0, "Written reply")["status"], "text_only");
        assert_eq!(
            c.room.history(Some("t"))["messages"][0]["hearing"],
            Value::Null
        );
        c.hearing("r", outcome, None);
        let row = &c.room.history(Some("t"))["messages"][0];
        assert_eq!(row["hearing"], outcome);
        assert_eq!(row["globally_heard"], outcome == "heard");
        assert_eq!(row["heard_chars"], if outcome == "heard" { 13 } else { 0 });
    }
}

#[test]
fn unconfirmed_history_is_bounded_without_fabricating_not_heard_evidence() {
    let c = Case::new(true);
    {
        let mut inner = c.room.inner.lock().unwrap();
        for n in 0..2001 {
            inner.append_row(crate::control::room::journal::Row {
                id: format!("unconfirmed-{n}"),
                thread: "t".into(),
                role: "assistant",
                text: "Written reply".into(),
                status: "text_only".into(),
                hearing_unconfirmed: true,
                ..Default::default()
            });
        }
    }
    let old_id = "unconfirmed-0".to_owned();
    let resolved = c
        .room
        .history_resolving(Some("t"), std::slice::from_ref(&old_id));
    assert_eq!(resolved["missing_ids"], json!([old_id]));
    assert!(!c.room.inner.lock().unwrap().unheard.has("t"));
    assert!(c.notes().is_empty());
    assert_eq!(c.publish("fresh", 0, "Fresh answer")["status"], "queued");
}

#[test]
fn semantic_hearing_uses_cross_device_max_and_monotonic_corrections() {
    let c = Case::new(true);
    let (sender, _events) = mpsc::channel(64);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "t").unwrap();
    c.publish("r", 0, "é🐙ab");
    let id = format!("{}:voice:r", c.sid);
    c.hearing("r", "not_heard", None);
    c.room
        .reply_hearing(&other, &id, "r", "not_heard", None)
        .unwrap();
    c.say("next", Some("Wait"));
    let original = c.pull()[0].clone();
    let reply = &original["unheard"]["replies"][0];
    assert_eq!(reply["fate"], "not_heard");
    assert_eq!(reply["heard_chars"], 0);
    assert!(reply.get("reason").is_none());
    assert!(reply.get("cut").is_none());
    c.room
        .reply_hearing(&other, &id, "r", "partial", Some(2))
        .unwrap();
    let first = c.pull();
    assert_eq!(first[0], original);
    assert_eq!(first[1]["notice"]["heard"][0]["fate"], "partial");
    assert_eq!(first[1]["notice"]["heard"][0]["heard_chars"], 2);
    c.hearing("r", "partial", Some(1));
    assert_eq!(c.pull().len(), 2, "a smaller prefix adds no correction");
    c.room
        .reply_hearing(&other, &id, "r", "heard", None)
        .unwrap();
    let final_messages = c.pull();
    assert_eq!(final_messages[2]["notice"]["heard"][0]["fate"], "heard");
    assert_eq!(final_messages[2]["notice"]["heard"][0]["heard_chars"], 4);
    c.hearing("r", "not_heard", None);
    assert_eq!(c.pull().len(), 3, "late weaker evidence cannot regress");
    let row = &c.room.history(Some("t"))["messages"][0];
    assert_eq!(row["hearing"], "heard");
    assert_eq!(row["heard_chars"], 4);
}

#[test]
fn semantic_hearing_requires_matching_identity_and_valid_extent() {
    let c = Case::new(true);
    c.publish("r", 0, "é🐙ab");
    let id = format!("{}:voice:r", c.sid);
    for (history_id, uid, outcome, extent, key) in [
        (id.as_str(), "wrong", "heard", None, "room.stale_utterance"),
        (id.as_str(), "r", "partial", None, "room.receipt_invalid"),
        (id.as_str(), "r", "partial", Some(4), "room.receipt_invalid"),
        (
            id.as_str(),
            "r",
            "not_heard",
            Some(0),
            "room.receipt_invalid",
        ),
        (id.as_str(), "r", "unknown", None, "room.receipt_invalid"),
    ] {
        assert_eq!(
            c.room
                .reply_hearing(&c.sid, history_id, uid, outcome, extent)
                .unwrap_err()
                .key,
            key
        );
    }
    assert_eq!(
        c.room
            .reply_hearing("absent-session", &id, "r", "heard", None)
            .unwrap_err()
            .key,
        "room.stale_utterance"
    );
    assert_eq!(
        c.room.history(Some("t"))["messages"][0]["hearing"],
        Value::Null
    );
}

#[test]
fn h4_saved_unavailable_history_can_be_acknowledged_from_other_or_unfocused_call() {
    let c = Case::new(true);
    let (publisher_sender, _publisher_events) = mpsc::channel(64);
    let publisher = c
        .room
        .join("publisher".into(), "en".into(), publisher_sender)
        .unwrap();
    c.room.select(&publisher, "saved-b").unwrap();
    c.room
        .set_speech(&publisher, false, Some("voice_off"))
        .unwrap();
    let revision = c.room.snapshot(Some(&publisher))["room"]["revision"]
        .as_u64()
        .unwrap();
    assert_eq!(
        c.room
            .publish(&json!({"session_id":publisher,"thread_id":"saved-b",
            "revision":revision,"epoch":0,"utterance_id":"saved",
            "text":"Saved reply"}))["status"],
        "text_only"
    );
    let history_id = format!("{publisher}:voice:saved");
    assert_eq!(
        c.room.history(Some("saved-b"))["messages"][0]["id"],
        history_id
    );
    c.room.leave(&publisher);
    assert_eq!(c.room.snapshot(Some(&c.sid))["binding"]["thread_id"], "t");

    let (new_sender, _new_events) = mpsc::channel(64);
    let new_session = c
        .room
        .join("reader".into(), "en".into(), new_sender)
        .unwrap();
    c.room
        .reply_hearing(&new_session, &history_id, "saved", "partial", Some(2))
        .unwrap();
    assert_eq!(
        c.room.history(Some("saved-b"))["messages"][0]["hearing"],
        "partial"
    );
    c.room
        .reply_hearing(&c.sid, &history_id, "saved", "heard", None)
        .unwrap();
    assert_eq!(
        c.room.history(Some("saved-b"))["messages"][0]["hearing"],
        "heard"
    );
    assert_eq!(
        c.room
            .reply_hearing(&new_session, &history_id, "wrong", "heard", None)
            .unwrap_err()
            .key,
        "room.stale_utterance"
    );
}

#[test]
fn h6_zero_partial_is_invalid_before_and_after_not_heard_exposure() {
    let c = Case::new(true);
    c.publish("r", 0, "Reply");
    let history_id = format!("{}:voice:r", c.sid);
    assert_eq!(
        c.room
            .reply_hearing(&c.sid, &history_id, "r", "partial", Some(0))
            .unwrap_err()
            .key,
        "room.receipt_invalid"
    );
    assert_eq!(
        c.room.history(Some("t"))["messages"][0]["hearing"],
        Value::Null
    );
    c.hearing("r", "not_heard", None);
    c.room
        .playback_transport(&c.sid, "r", "interrupted", None, Some(0), &Value::Null)
        .unwrap();
    assert_eq!(
        c.room
            .reply_hearing(&c.sid, &history_id, "r", "partial", Some(0))
            .unwrap_err()
            .key,
        "room.receipt_invalid"
    );
    c.say("turn", Some("Wait"));
    let before = c.input("Wait");
    assert_eq!(before["unheard"]["replies"][0]["fate"], "not_heard");
    assert_eq!(
        c.room
            .reply_hearing(&c.sid, &history_id, "r", "partial", Some(0))
            .unwrap_err()
            .key,
        "room.receipt_invalid"
    );
    let exposed = c.pull();
    assert_eq!(exposed.len(), 1);
    assert_eq!(exposed[0]["unheard"]["replies"][0]["fate"], "not_heard");
    assert_eq!(
        c.room
            .reply_hearing(&c.sid, &history_id, "r", "partial", Some(0))
            .unwrap_err()
            .key,
        "room.receipt_invalid"
    );
    assert_eq!(c.pull(), exposed);
    let row = &c.room.history(Some("t"))["messages"][0];
    assert_eq!(row["hearing"], "not_heard");
    assert_eq!(row["heard_chars"], 0);
}

#[test]
fn f1_uncertain_publication_recovers_without_late_first_audio_and_fresh_reply_plays() {
    let mut c = Case::new(true);
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../docs/fixtures/conversation-flow.json"
    ))
    .unwrap();
    assert_eq!(fixture["speech_recovery"]["method"], "speech.recover");
    let original = json!({"event_id":"event-a","utterance_id":"a","binding_id":c.bid,
        "session_id":"lost-session","revision":3,"epoch":0,"text":"Older answer","history_only":false});
    c.room.unregister("connector", &c.bid);
    let current_binding = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"codex",
        "input_mode":"pull"}),
        )
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(current_binding, c.bid);
    let mut recovery = original.clone();
    recovery["recovery_binding_id"] = json!(current_binding);
    let saved = c.room.recover_speech("connector", &recovery);
    for key in ["status", "reason", "text_saved"] {
        assert_eq!(
            saved[key],
            fixture["speech_recovery"]["missing_first_admission"][key]
        );
    }
    assert_eq!(c.room.recover_speech("connector", &recovery), saved);
    let mut original_with_thread = original.clone();
    original_with_thread["thread_id"] = json!("t");
    assert_eq!(c.room.publish(&original_with_thread), saved);
    assert!(c.frames("voice-reply").is_empty());
    let mut changed = recovery.clone();
    changed["history_only"] = json!(true);
    assert_eq!(
        c.room.recover_speech("connector", &changed)["error"],
        "room.speech_conflict"
    );
    c.say("next", Some("Fresh question"));
    assert_eq!(c.publish("b", 1, "Fresh answer")["status"], "queued");
    assert_eq!(c.frames("voice-reply").len(), 1);
}

#[test]
fn f1_recovery_returns_original_saved_or_refused_disposition() {
    let c = Case::new(true);
    let mut saved_request = json!({"event_id":"event-saved","utterance_id":"saved",
        "binding_id":c.bid,"session_id":c.sid,"revision":c.revision(),"epoch":0,
        "text":"Already admitted","history_only":false});
    let admitted = c.room.connector_speech("connector", &saved_request);
    saved_request["recovery_binding_id"] = json!(c.bid);
    assert_eq!(c.room.recover_speech("connector", &saved_request), admitted);
    let turn = c.room.begin_at_focus(&c.sid, "open").unwrap();
    let mut refused_request = json!({"event_id":"event-refused","utterance_id":"refused",
        "binding_id":c.bid,"session_id":c.sid,"revision":c.revision(),"epoch":turn.epoch,
        "text":"During speech","history_only":false});
    let refused = c.room.connector_speech("connector", &refused_request);
    assert_eq!(refused["error"], "room.user_speaking");
    refused_request["recovery_binding_id"] = json!(c.bid);
    assert_eq!(
        c.room.recover_speech("connector", &refused_request),
        refused
    );
}

fn missed_note_with_late_other_device(pull: bool) -> (Case, String) {
    let c = Case::new(pull);
    let (sender, _events) = mpsc::channel(64);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "t").unwrap();
    c.publish("r", 0, "Missed answer");
    c.hearing("r", "not_heard", None);
    c.room
        .reply_hearing(
            &other,
            &format!("{}:voice:r", c.sid),
            "r",
            "not_heard",
            None,
        )
        .unwrap();
    c.room.park(&c.sid, true);
    c.room.park(&other, true);
    c.room.resume(&c.sid, &[]);
    c.room
        .inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    (c, other)
}

#[test]
fn f4_lost_push_ack_keeps_exposed_notice_frozen_and_sends_hearing_correction() {
    let (c, other) = missed_note_with_late_other_device(false);
    let first = c.room.pending_delivery();
    assert_eq!(first.len(), 1);
    let (bid, rid, peer, offered) = &first[0];
    let first_id = offered["message_id"].clone();
    let first_payload = c.notes()[0].clone();
    c.room
        .settle_delivery(bid, rid, &peer.generation, Ok(json!({"status":"failed"})));
    assert_eq!(
        c.room
            .inner
            .lock()
            .unwrap()
            .journal
            .find(rid)
            .unwrap()
            .status,
        "pending"
    );
    c.room
        .playback(&other, "r", "heard", None, None, &Value::Null)
        .unwrap();
    let notes = c.notes();
    assert_eq!(notes[0], first_payload);
    assert_eq!(notes[1]["notice"]["kind"], "reply_heard");
    assert_eq!(notes[1]["notice"]["corrects_message_id"], first_id);
    assert_eq!(notes[1]["notice"]["heard"][0]["fate"], "heard");
    {
        let mut inner = c.room.inner.lock().unwrap();
        inner.journal.find_mut(rid).unwrap().next_attempt = 0;
    }
    let retry = c.room.pending_delivery();
    assert_eq!(retry[0].3["message_id"], first_id);
    assert_eq!(retry[0].3["unheard"], offered["unheard"]);
}

#[test]
fn f4_released_pull_claim_keeps_original_payload_and_corrects_partial_or_full() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../docs/fixtures/conversation-flow.json"
    ))
    .unwrap();
    assert_eq!(fixture["notice_correction"]["kind"], "reply_heard");
    for outcome in ["partial", "heard"] {
        let (c, other) = missed_note_with_late_other_device(true);
        let first = c.pull();
        assert_eq!(first.len(), 1);
        let original = first[0].clone();
        let original_payload = c.notes()[0].clone();
        c.room
            .inner
            .lock()
            .unwrap()
            .release_pull_claims(|row| row.pull_claimed_by.as_deref() == Some(c.bid.as_str()));
        match outcome {
            "partial" => c
                .room
                .playback(
                    &other,
                    "r",
                    "interrupted",
                    Some("user_interrupted"),
                    Some(2),
                    &Value::Null,
                )
                .unwrap(),
            "heard" => c
                .room
                .playback(&other, "r", "heard", None, None, &Value::Null)
                .unwrap(),
            _ => unreachable!(),
        }
        assert_eq!(c.notes()[0], original_payload, "{outcome}");
        let messages = c.pull();
        assert_eq!(messages[0], original, "{outcome}");
        let correction = messages
            .iter()
            .find(|m| m["notice"]["kind"] == "reply_heard")
            .unwrap();
        assert_eq!(
            correction["notice"]["corrects_message_id"],
            original["message_id"]
        );
        assert_eq!(correction["notice"]["heard"][0]["fate"], outcome);
        if outcome == "partial" {
            assert_eq!(correction["notice"]["heard"][0]["heard_chars"], 2);
            c.room
                .playback(&other, "r", "heard", None, None, &Value::Null)
                .unwrap();
            let after_full = c.pull();
            assert!(after_full.iter().any(|m| {
                m["notice"]["corrects_message_id"] == original["message_id"]
                    && m["notice"]["heard"][0]["fate"] == "heard"
            }));
            assert_eq!(c.notes()[0], original_payload);
        }
    }
}

#[test]
fn f2_original_reply_final_receipt_after_park_resume_updates_global_hearing() {
    let c = Case::new(true);
    c.publish("original", 0, "Original reply");
    c.room.park(&c.sid, true);
    c.room.resume(&c.sid, &[]);
    c.report("original", "heard", None, None);
    let row = &c.room.history(Some("t"))["messages"][0];
    assert_eq!(row["globally_heard"], true);
    assert_eq!(row["heard_chars"], 14);
}

#[test]
fn f2_manual_replay_late_semantic_receipts_after_park_resume_reconcile_original_and_notices() {
    for (status, reason, extent, fate) in [
        ("heard", None, None, "heard"),
        ("interrupted", Some("user_interrupted"), Some(3), "partial"),
    ] {
        let c = Case::new(true);
        c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
        c.publish("original", 0, "Replay me");
        c.hearing("original", "not_heard", None);
        c.room.set_speech(&c.sid, true, None).unwrap();
        c.room
            .inner
            .lock()
            .unwrap()
            .unheard
            .note_mut("t")
            .unwrap()
            .due = 0;
        let first = c.pull();
        let prior = first[0]["message_id"].clone();
        let history_id = c.room.history(Some("t"))["messages"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        c.room.replay_one(&c.sid, &history_id, "manual").unwrap();
        c.room.park(&c.sid, true);
        c.room.resume(&c.sid, &[]);
        assert!(c.room.inner.lock().unwrap().utterances.contains("manual"));
        c.report("manual", status, reason, extent);
        let row = &c.room.history(Some("t"))["messages"][0];
        assert_eq!(row["globally_heard"], fate != "partial", "{fate}");
        assert_eq!(
            row["heard_chars"],
            if fate == "partial" {
                json!(3)
            } else {
                json!(9)
            }
        );
        let messages = c.pull();
        assert!(
            messages.iter().any(|message| {
                message["notice"]["corrects_message_id"] == prior
                    && message["notice"]["heard"][0]["fate"] == fate
            }),
            "{fate}: {messages:?}"
        );
    }
}

#[test]
fn f2_finished_replay_receipt_identities_are_bounded_without_blocking_new_replays() {
    let mut c = Case::new(true);
    c.publish("original", 0, "Reply");
    c.report("original", "heard", None, None);
    let history_id = c.room.history(Some("t"))["messages"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for n in 0..260 {
        let uid = format!("manual-{n}");
        c.room.replay_one(&c.sid, &history_id, &uid).unwrap();
        c.report(&uid, "heard", None, None);
        c.frames("voice-reply");
    }
    let inner = c.room.inner.lock().unwrap();
    assert_eq!(inner.utterances.replay_count(), 0);
    assert_eq!(
        inner
            .utterances
            .iter()
            .filter(|(_, r)| r.is_replay())
            .count(),
        256
    );
    assert!(!inner.utterances.contains("manual-0"));
    assert!(inner.utterances.contains("manual-259"));
}

#[test]
fn t01_playing_reply_cut_carries_full_unicode_evidence_with_the_words() {
    let c = Case::new(false);
    assert_eq!(c.publish("r", 0, "é🐙 rest")["status"], "queued");
    c.report("r", "playing", None, None);
    c.report("r", "interrupted", Some("user_interrupted"), Some(2));
    assert_eq!(c.say("turn", Some("Wait")), 1);
    let payload = c.input("Wait");
    assert_eq!(payload["notice"]["kind"], "reply_not_heard");
    let reply = &payload["unheard"]["replies"][0];
    assert_eq!(
        (&reply["text"], &reply["heard_chars"], &reply["fate"]),
        (&json!("é🐙 rest"), &json!(2), &json!("partial"))
    );
}

#[test]
fn t02a_accepted_before_start_is_withdrawn_and_attached_to_words() {
    let mut c = Case::new(false);
    assert_eq!(c.publish("r", 0, "Not yet started")["status"], "queued");
    c.frames("voice-reply");
    let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
    assert_eq!(turn.epoch, 1);
    assert_eq!(c.frames("voice-reply-withdrawn").len(), 1);
    c.report("r", "unplayed", Some("newer_turn"), None);
    c.room
        .finish_turn(
            &c.sid,
            "turn",
            Some("Actually"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(
        c.input("Actually")["unheard"]["replies"][0]["fate"],
        "not_heard"
    );
    assert!(c.input("Actually")["unheard"]["replies"][0]
        .get("reason")
        .is_none());
    assert!(c.frames("voice-reply").is_empty());
}

#[test]
fn t02b_accepted_before_start_with_empty_turn_queues_empty_notice() {
    let c = Case::new(true);
    c.publish("r", 0, "Not played");
    c.room.begin_at_focus(&c.sid, "turn").unwrap();
    c.report("r", "unplayed", Some("newer_turn"), None);
    c.room
        .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
        .unwrap();
    let notes = c.notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["text"], "");
    assert_eq!(notes[0]["notice"]["kind"], "turn_empty");
    assert_eq!(notes[0]["unheard"]["replies"][0]["text"], "Not played");
    assert!(c.pull().iter().any(|message| message["channel"] == "note"));
}

#[test]
fn t02c_refusal_with_words_is_terminal_and_does_not_create_a_reply() {
    let c = Case::new(false);
    let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
    let refused = c.publish("r", turn.epoch, "Attempt");
    assert_eq!(refused["reason"], "user_speaking");
    c.room
        .finish_turn(
            &c.sid,
            "turn",
            Some("Words"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(c.publish("r", turn.epoch, "Attempt"), refused);
    assert_eq!(c.input("Words")["text"], "Words");
    assert!(c.room.history(Some("t"))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["role"] != "assistant"));
}

#[test]
fn t02d_refusal_with_no_words_queues_the_said_nothing_instruction() {
    let c = Case::new(true);
    let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
    c.publish("r", turn.epoch, "Attempt");
    c.room
        .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
        .unwrap();
    assert_eq!(c.notes().len(), 1);
    assert!(c.notes()[0]["notice"]["instruction"]
        .as_str()
        .unwrap()
        .contains("said nothing"));
    assert_eq!(c.pull()[0]["channel"], "note");
}

#[test]
fn t03_late_old_epoch_reply_stays_written_and_notifies_without_another_turn() {
    let mut c = Case::new(false);
    c.say("new", Some("New question"));
    let answer = c.publish("late", 0, "Old answer");
    assert_eq!(answer["status"], "text_only");
    assert!(answer.get("reason").is_none());
    assert_eq!(
        c.room.history(Some("t"))["messages"][1]["audio_reason"],
        "newer_turn"
    );
    assert!(c.frames("voice-reply").is_empty());
    c.hearing("late", "not_heard", None);
    assert_eq!(c.notes()[0]["unheard"]["replies"][0]["text"], "Old answer");
}

#[test]
fn t05_checkpoint_interruption_carries_its_cut_extent() {
    let mut c = Case::new(false);
    for id in ["ack", "checkpoint", "result"] {
        c.publish(id, 0, id);
    }
    let ids: Vec<_> = c
        .frames("voice-reply")
        .iter()
        .map(|frame| frame["data"]["utterance_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["ack", "checkpoint", "result"]);
    c.report("ack", "heard", None, None);
    c.report("checkpoint", "playing", None, None);
    c.report(
        "checkpoint",
        "interrupted",
        Some("user_interrupted"),
        Some(4),
    );
    c.say("next", Some("Stop there"));
    assert_eq!(
        c.input("Stop there")["unheard"]["replies"][0]["utterance_id"],
        "checkpoint"
    );
}

#[test]
fn t04a_multiple_same_epoch_replies_remain_fifo_and_present() {
    let mut c = Case::new(false);
    for id in ["first", "second", "third"] {
        c.publish(id, 0, id);
    }
    let ids: Vec<_> = c
        .frames("voice-reply")
        .iter()
        .map(|frame| frame["data"]["utterance_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["first", "second", "third"]);
    for id in ["first", "second", "third"] {
        c.report(id, "heard", None, None);
    }
    assert_eq!(
        c.room.history(Some("t"))["messages"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(c.notes().is_empty());
}

#[test]
fn t04b_new_epoch_withdraws_pending_replies_on_both_devices() {
    let mut c = Case::new(false);
    let (sender, mut other_events) = mpsc::channel(64);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "t").unwrap();
    c.publish("one", 0, "One");
    c.publish("two", 0, "Two");
    c.frames("voice-reply");
    while other_events.try_recv().is_ok() {}
    c.room.begin_at_focus(&c.sid, "turn").unwrap();
    let first = c.frames("voice-reply-withdrawn");
    let second: Vec<_> = std::iter::from_fn(|| other_events.try_recv().ok())
        .filter(|frame| frame["type"] == "voice-reply-withdrawn")
        .collect();
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(
        first[0]["data"]["utterance_ids"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        second[0]["data"]["utterance_ids"].as_array().unwrap().len(),
        2
    );
}

fn output_off_case(reason: &str) {
    let mut c = Case::new(false);
    c.publish("playing", 0, "Partly heard");
    c.report("playing", "playing", None, None);
    c.frames("voice-reply");
    c.room.set_speech(&c.sid, false, Some(reason)).unwrap();
    assert_eq!(c.frames("voice-reply-withdrawn").len(), 1);
    c.report("playing", "interrupted", Some(reason), Some(3));
    let answer = c.publish("later", 0, "Not sounded");
    assert_eq!(answer["status"], "text_only");
    assert!(answer.get("reason").is_none());
    c.room.set_speech(&c.sid, true, None).unwrap();
    assert!(c.frames("voice-reply").is_empty());
    let history = c.room.history(Some("t"));
    assert_eq!(history["messages"][0]["heard_chars"], 3);
    assert_eq!(history["messages"][1]["audio_reason"], reason);
    assert_eq!(history["messages"][1]["globally_heard"], false);
}

#[test]
fn t06a_muting_agents_during_playback_saves_partial_extent() {
    output_off_case("agents_muted");
}

#[test]
fn t06b_voice_off_saves_text_without_resuming_old_audio() {
    output_off_case("voice_off");
}

#[test]
fn t06c_focus_switch_keeps_captured_words_in_their_original_conversation() {
    let c = Case::new(false);
    let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
    c.room.select(&c.sid, "other").unwrap();
    c.room
        .finish_turn(
            &c.sid,
            "turn",
            Some("Original"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(turn.thread_id.as_deref(), Some("t"));
    assert_eq!(c.input("Original")["thread_id"], "t");
    assert_eq!(
        c.room.participants(Some(&c.sid)).as_array().unwrap()[0]["epoch"],
        1
    );
}

#[test]
fn t06d_return_with_misses_creates_one_notice_after_grace_for_pull() {
    let c = Case::new(true);
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.publish("missed", 0, "A missed update");
    c.hearing("missed", "not_heard", None);
    c.room.set_speech(&c.sid, true, None).unwrap();
    assert!(c.notes().is_empty());
    c.room
        .inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    let first = c.pull();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["channel"], "note");
    assert_eq!(first[0]["notice"]["kind"], "user_returned");
    assert_eq!(first[0]["unheard"]["replies"][0]["text"], "A missed update");
    assert_eq!(c.pull()[0]["message_id"], first[0]["message_id"]);
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.room.set_speech(&c.sid, true, None).unwrap();
    assert_eq!(
        c.notes().len(),
        1,
        "returning again does not duplicate an untold batch"
    );
}

#[test]
fn t06e_return_without_misses_or_after_global_hearing_has_no_notice() {
    let c = Case::new(true);
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.room.set_speech(&c.sid, true, None).unwrap();
    assert!(c.notes().is_empty());
    c.publish("heard", 0, "All heard");
    c.report("heard", "heard", None, None);
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.room.set_speech(&c.sid, true, None).unwrap();
    assert!(c.notes().is_empty());
    assert_eq!(c.room.participants(Some(&c.sid))[0]["unheard"], Value::Null);
}

#[test]
fn t07_disconnect_and_resume_never_replays_a_received_but_unstarted_reply() {
    let mut c = Case::new(false);
    c.publish("queued", 0, "Queued before link loss");
    c.frames("voice-reply");
    c.room.park(&c.sid, true);
    assert_eq!(
        c.room.history(Some("t"))["messages"][0]["status"],
        "interrupted"
    );
    c.room.resume(&c.sid, &[]);
    assert!(c.frames("voice-reply").is_empty());
    c.report("queued", "unplayed", Some("unheard"), None);
    assert_eq!(
        c.room.history(Some("t"))["messages"][0]["globally_heard"],
        false
    );
}

#[test]
fn t08_typed_context_during_an_open_turn_preserves_epoch_and_pending_evidence() {
    let c = Case::new(false);
    c.publish("r", 0, "Unheard");
    let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
    c.report("r", "unplayed", Some("newer_turn"), None);
    let binding = c.room.select(&c.sid, "t").unwrap()["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let typed = c
        .room
        .send_text("Aside", &c.sid, "t", &binding, "typed-1")
        .unwrap();
    assert_eq!(typed["epoch"], turn.epoch);
    assert!(c.input("Aside").get("unheard").is_none());
    assert!(c.room.inner.lock().unwrap().unheard.has("t"));
    c.room
        .finish_turn(
            &c.sid,
            "turn",
            Some("Spoken"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(c.input("Spoken")["unheard"]["count"], 1);
}

#[test]
fn t09b_short_word_in_silence_opens_an_ordinary_epoch() {
    let c = Case::new(false);
    assert_eq!(c.say("short", Some("vale")), 1);
    assert_eq!(c.input("vale")["epoch"], 1);
}

#[test]
fn t13_mic_mute_terminal_silence_uses_the_ordinary_empty_outcome() {
    let c = Case::new(false);
    let second = c.room.begin_at_focus(&c.sid, "mic-muted").unwrap();
    assert_eq!(second.epoch, 1);
    c.room
        .finish_turn(
            &c.sid,
            "mic-muted",
            None,
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(
        c.notes().len(),
        0,
        "empty without a missed/refused obligation is silent"
    );
}

#[test]
fn t11_threads_sessions_and_duplicate_starts_keep_separate_epochs() {
    let c = Case::new(false);
    let (sender, _events) = mpsc::channel(32);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "other").unwrap();
    let a = c.room.begin_at_focus(&c.sid, "a").unwrap();
    let b = c.room.begin_at_focus(&other, "b").unwrap();
    assert_eq!((a.epoch, b.epoch), (1, 1));
    let duplicate = c.room.begin_at_focus(&c.sid, "a").unwrap();
    assert_eq!(duplicate.epoch, 1);
    c.room
        .finish_turn(
            &other,
            "b",
            Some("Other thread"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    c.room
        .finish_turn(
            &c.sid,
            "a",
            Some("This thread"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(c.input("This thread")["epoch"], 1);
    assert_eq!(c.room.history(Some("other"))["messages"][0]["epoch"], 1);
}

#[test]
fn t12_local_skip_can_report_ordinary_full_hearing_without_staling_next_reply() {
    let mut c = Case::new(true);
    c.publish("first", 0, "Skip this");
    c.publish("next", 0, "Play this");
    c.frames("voice-reply");
    c.room
        .playback_transport(&c.sid, "first", "interrupted", None, Some(2), &Value::Null)
        .unwrap();
    c.hearing("first", "heard", None);
    let history = c.room.history(Some("t"));
    assert_eq!(history["messages"][0]["status"], "interrupted");
    assert_eq!(history["messages"][0]["hearing"], "heard");
    assert_eq!(history["messages"][0]["globally_heard"], true);
    assert_eq!(history["messages"][1]["status"], "queued");
    assert!(c.notes().is_empty());
}

#[test]
fn t14_global_full_hearing_wins_and_partial_extents_take_the_maximum() {
    let c = Case::new(false);
    let (sender, _events) = mpsc::channel(64);
    let second = c.room.join("second".into(), "en".into(), sender).unwrap();
    c.room.select(&second, "t").unwrap();
    c.publish("partial", 0, "é🐙abcdef");
    c.report("partial", "playing", None, None);
    c.report("partial", "interrupted", None, Some(3));
    c.room
        .playback(&second, "partial", "playing", None, None, &Value::Null)
        .unwrap();
    c.room
        .playback(
            &second,
            "partial",
            "interrupted",
            None,
            Some(5),
            &Value::Null,
        )
        .unwrap();
    assert_eq!(c.room.history(Some("t"))["messages"][0]["heard_chars"], 5);
    c.publish("whole", 0, "Complete");
    c.report("whole", "heard", None, None);
    c.room
        .playback(&second, "whole", "failed", None, None, &Value::Null)
        .unwrap();
    assert_eq!(
        c.room.history(Some("t"))["messages"][1]["globally_heard"],
        true
    );
}

#[test]
fn t16_pull_claims_empty_notices_until_ack_and_push_uses_same_content() {
    for pull in [false, true] {
        let c = Case::new(pull);
        let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
        c.publish("refused", turn.epoch, "Attempt");
        c.room
            .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
            .unwrap();
        if pull {
            let first = c.pull();
            let again = c.pull();
            assert_eq!(first, again);
            let id = first[0]["message_id"].clone();
            let ack = c
                .room
                .pull_input(
                    "connector",
                    &json!({"binding_id":c.bid,
                "operation":"get","ack_ids":[id]}),
                )
                .unwrap();
            assert_eq!(ack["acknowledged"], 1);
        } else {
            let due = c.room.pending_delivery();
            assert_eq!(due.len(), 1);
            assert_eq!(due[0].3["channel"], "note");
            assert_eq!(due[0].3["notice"]["kind"], "turn_empty");
        }
    }
}

#[test]
fn t07_connector_held_publication_is_explicit_history_only_after_reconnect() {
    let mut c = Case::new(false);
    let mut request = json!({"session_id":c.sid,"thread_id":"t","revision":c.revision(),
        "epoch":0,"utterance_id":"held","text":"Held across disconnect","history_only":true});
    let first = c.room.publish(&request);
    assert_eq!(first["status"], "text_only");
    assert!(first.get("reason").is_none());
    request["history_only"] = json!(false);
    assert_eq!(c.room.publish(&request)["error"], "room.speech_conflict");
    assert!(c.frames("voice-reply").is_empty());
}

#[test]
fn t11_merged_turn_transfers_the_empty_obligation_to_its_successor() {
    let c = Case::new(true);
    let first = c.room.begin_at_focus(&c.sid, "first").unwrap();
    c.publish("refused", first.epoch, "Attempt");
    let second = c.room.begin_at_focus(&c.sid, "second").unwrap();
    c.room
        .finish_turn(&c.sid, "first", None, &Value::Null, Continuation::Merged)
        .unwrap();
    assert!(c.notes().is_empty());
    c.room
        .finish_turn(
            &c.sid,
            "second",
            None,
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(c.notes().len(), 1);
    assert_eq!(c.notes()[0]["notice"]["kind"], "turn_empty");
    assert_eq!(second.epoch, 2);
}

#[test]
fn t11_offline_input_uses_the_captured_target_after_focus_changes() {
    let c = Case::new(false);
    let binding = c.room.select(&c.sid, "t").unwrap()["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    c.room.select(&c.sid, "other").unwrap();
    let accepted = c
        .room
        .offline_input_captured(&c.sid, "offline", "Captured words", None, "t", &binding)
        .unwrap();
    assert_eq!(accepted["epoch"], 1);
    assert_eq!(c.input("Captured words")["thread_id"], "t");
    let again = c
        .room
        .offline_input_captured(&c.sid, "offline", "Captured words", None, "t", &binding)
        .unwrap();
    assert_eq!(again["epoch"], 1);
}

#[test]
fn t15_core_exposes_working_facts_without_changing_epoch() {
    let c = Case::new(false);
    assert_eq!(c.room.participants(Some(&c.sid))[0]["working"], Value::Null);
    c.room
        .working("connector", &json!({"binding_id":c.bid,"working":true}));
    assert_eq!(c.room.participants(Some(&c.sid))[0]["working"], true);
    assert_eq!(c.room.participants(Some(&c.sid))[0]["epoch"], 0);
    c.room
        .working("connector", &json!({"binding_id":c.bid,"working":false}));
    assert_eq!(c.room.participants(Some(&c.sid))[0]["working"], false);
}

#[test]
fn t16_unacked_note_survives_pull_binding_replacement_with_the_same_id() {
    let c = Case::new(true);
    let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
    c.publish("refused", turn.epoch, "Attempt");
    c.room
        .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
        .unwrap();
    let original = c.pull()[0]["message_id"].clone();
    let replacement = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"codex",
        "input_mode":"pull","binding_id":"replacement"}),
        )
        .unwrap();
    let bid = replacement["binding_id"].as_str().unwrap();
    let fetched = c
        .room
        .pull_input("connector", &json!({"binding_id":bid,"operation":"get"}))
        .unwrap();
    assert_eq!(fetched["messages"][0]["message_id"], original);
    assert_eq!(fetched["messages"][0]["channel"], "note");
}

#[test]
fn t02a_late_receipt_after_the_spoken_message_becomes_a_correlated_followup() {
    let c = Case::new(true);
    c.publish("r", 0, "Late receipt");
    c.say("new", Some("New words"));
    assert!(c.input("New words").get("unheard").is_none());
    c.report("r", "unplayed", Some("newer_turn"), None);
    let notes = c.notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["notice"]["kind"], "reply_not_heard");
    assert_eq!(
        notes[0]["notice"]["related_message_id"],
        c.input("New words")["message_id"]
    );
    assert_eq!(c.pull()[1]["channel"], "note");
}

#[test]
fn t06e_second_device_hearing_removes_the_first_devices_false_return_notice() {
    let c = Case::new(true);
    let (sender, _events) = mpsc::channel(64);
    let second = c.room.join("second".into(), "en".into(), sender).unwrap();
    c.room.select(&second, "t").unwrap();
    c.publish("r", 0, "Heard on second");
    c.room
        .playback(&second, "r", "heard", None, None, &Value::Null)
        .unwrap();
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.room.set_speech(&c.sid, true, None).unwrap();
    assert!(c.notes().is_empty());
    assert_eq!(c.room.participants(Some(&c.sid))[0]["unheard"], Value::Null);
}

#[test]
fn t11_one_devices_finish_does_not_close_another_devices_open_turn() {
    let c = Case::new(true);
    let (sender, _events) = mpsc::channel(64);
    let second = c.room.join("second".into(), "en".into(), sender).unwrap();
    c.room.select(&second, "t").unwrap();
    let first = c.room.begin_at_focus(&c.sid, "first").unwrap();
    let other = c.room.begin_at_focus(&second, "other").unwrap();
    c.room
        .finish_turn(
            &c.sid,
            "first",
            Some("First"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(
        c.publish("attempt", other.epoch, "Still open")["reason"],
        "user_speaking"
    );
    c.room
        .finish_turn(
            &second,
            "other",
            None,
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!((first.epoch, other.epoch), (1, 2));
}

#[test]
fn t16_full_evidence_is_batched_eight_at_a_time_without_truncating_replies() {
    let c = Case::new(true);
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    for n in 0..9 {
        let text = format!("{n}{}TAIL", "é".repeat(300));
        assert_eq!(c.publish(&format!("r{n}"), 0, &text)["status"], "text_only");
        c.hearing(&format!("r{n}"), "not_heard", None);
    }
    c.room.set_speech(&c.sid, true, None).unwrap();
    c.room
        .inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    let messages = c.pull();
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0]["unheard"]["replies"].as_array().unwrap().len(),
        8
    );
    assert_eq!(
        messages[1]["unheard"]["replies"].as_array().unwrap().len(),
        1
    );
    assert!(messages[0]["unheard"]["replies"][0]["text"]
        .as_str()
        .unwrap()
        .ends_with("TAIL"));
    assert!(messages[1]["unheard"]["replies"][0]["text"]
        .as_str()
        .unwrap()
        .ends_with("TAIL"));
}

#[test]
fn t16_evidence_batches_keep_publication_order_when_receipts_arrive_backwards() {
    let c = Case::new(true);
    for n in 0..9 {
        assert_eq!(c.publish(&format!("r{n}"), 0, "Reply")["status"], "queued");
    }
    for n in (0..9).rev() {
        c.report(&format!("r{n}"), "failed", None, None);
    }
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.room.set_speech(&c.sid, true, None).unwrap();
    c.room
        .inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    let ids: Vec<String> = c
        .pull()
        .iter()
        .flat_map(|message| message["unheard"]["replies"].as_array().unwrap())
        .map(|reply| reply["utterance_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, (0..9).map(|n| format!("r{n}")).collect::<Vec<_>>());
}

#[test]
fn t16_full_input_queue_backpressures_new_speech_before_evidence_can_be_owed() {
    let c = Case::new(true);
    let focus = c.room.select(&c.sid, "t").unwrap();
    let focus_id = focus["binding"]["binding_id"].as_str().unwrap();
    for n in 0..256 {
        c.room
            .send_text("Queued input", &c.sid, "t", focus_id, &format!("input-{n}"))
            .unwrap();
    }
    let result = c.publish("would-owe-evidence", 0, "Reply");
    assert_eq!(result["status"], "rejected");
    assert_eq!(result["error"], "room.input_backlog_full");
    assert!(c.room.history(Some("t"))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["role"] != "assistant"));
}
#[test]
fn review_ack_releases_backlog_capacity() {
    let c = Case::new(true);
    c.say("one", Some("One"));
    let messages = c.pull();
    c.room
        .pull_input(
            "connector",
            &json!({"binding_id":c.bid,"operation":"get","ack_ids":[messages[0]["message_id"]]}),
        )
        .unwrap();
    assert_eq!(
        c.room.inner.lock().unwrap().journal.waiting_input().0,
        0,
        "acknowledged input must release capacity"
    );
}

#[test]
fn t16_pull_ack_cycles_release_capacity_for_more_input_and_speech() {
    let c = Case::new(true);
    let focus = c.room.select(&c.sid, "t").unwrap();
    let focus_id = focus["binding"]["binding_id"].as_str().unwrap();
    for n in 0..300 {
        c.room
            .send_text("Context", &c.sid, "t", focus_id, &format!("cycle-{n}"))
            .unwrap();
        let messages = c.pull();
        assert_eq!(messages.len(), 1, "cycle {n}");
        c.room
            .pull_input(
                "connector",
                &json!({"binding_id":c.bid,"operation":"get",
                    "ack_ids":[messages[0]["message_id"]]}),
            )
            .unwrap();
    }
    assert_eq!(c.room.inner.lock().unwrap().journal.waiting_input().0, 0);
    assert_eq!(c.publish("after-acks", 0, "Answer")["status"], "queued");
}

#[test]
fn review_new_epoch_does_not_withdraw_other_conversation() {
    let mut c = Case::new(false);
    c.publish("a-heard", 0, "Already heard in A");
    c.report("a-heard", "heard", None, None);
    c.room.select(&c.sid, "b").unwrap();
    assert_eq!(c.room.publish(&json!({"session_id":c.sid,"thread_id":"b","revision":c.revision(),"epoch":0,"utterance_id":"b-queued","text":"Current B reply"}))["status"], "queued");
    c.frames("voice-reply");
    let (sender, _events) = mpsc::channel(64);
    let a_device = c.room.join("a-device".into(), "en".into(), sender).unwrap();
    c.room.select(&a_device, "t").unwrap();
    c.room.begin_at_focus(&a_device, "new-a").unwrap();
    assert!(
        c.frames("voice-reply-withdrawn").is_empty(),
        "A's epoch must not withdraw B's reply"
    );
}

#[test]
fn review_late_receipt_after_empty_turn_queues_notice() {
    let c = Case::new(true);
    c.publish("r", 0, "Never played");
    c.say("empty", None);
    c.report("r", "unplayed", Some("newer_turn"), None);
    assert_eq!(
        c.pull().len(),
        1,
        "late evidence after empty turn must be delivered"
    );
}

#[test]
fn review_duplicate_cut_receipt_does_not_renotify() {
    let c = Case::new(true);
    c.publish("r", 0, "Partly heard");
    c.report("r", "interrupted", Some("user_interrupted"), Some(3));
    c.say("next", Some("Wait"));
    assert_eq!(c.pull().len(), 1);
    c.report("r", "interrupted", Some("user_interrupted"), Some(3));
    assert_eq!(
        c.pull().len(),
        1,
        "identical terminal evidence must not create another notice"
    );
}

#[test]
fn review_offline_epoch_withdraws_other_devices_pending_reply() {
    let mut c = Case::new(false);
    c.publish("r", 0, "Pending before offline input");
    c.frames("voice-reply");
    let focus = c.room.select(&c.sid, "t").unwrap();
    c.room
        .offline_input_captured(
            &c.sid,
            "offline",
            "New speech",
            None,
            "t",
            focus["binding"]["binding_id"].as_str().unwrap(),
        )
        .unwrap();
    assert_eq!(c.frames("voice-reply-withdrawn").len(), 1);
}

#[test]
fn review_return_before_terminal_receipt_still_schedules_notice() {
    let c = Case::new(true);
    c.publish("r", 0, "Mute stops me");
    c.report("r", "playing", None, None);
    c.room
        .set_speech(&c.sid, false, Some("agents_muted"))
        .unwrap();
    c.room.set_speech(&c.sid, true, None).unwrap();
    c.report("r", "interrupted", Some("agents_muted"), Some(2));
    assert!(
        c.room.inner.lock().unwrap().unheard.note_mut("t").is_some(),
        "return notice owed even if receipt arrived after return"
    );
}

#[test]
fn t06a_mute_cut_reason_follows_playback_receipt_in_either_frame_order() {
    for receipt_before_mute in [false, true] {
        let c = Case::new(true);
        assert_eq!(c.publish("playing", 0, "Mute stops me")["status"], "queued");
        c.report("playing", "playing", None, None);
        if receipt_before_mute {
            c.report("playing", "interrupted", Some("agents_muted"), Some(2));
        }
        c.room
            .set_speech(&c.sid, false, Some("agents_muted"))
            .unwrap();
        if !receipt_before_mute {
            c.report("playing", "interrupted", Some("agents_muted"), Some(2));
        }
        let rows = c.room.history(Some("t"));
        let row = &rows["messages"][0];
        assert_eq!(row["status"], "interrupted");
        assert_eq!(row["audio_reason"], "agents_muted");
        assert_eq!(row["heard_chars"], 2);
        let second = c.publish("later", 0, "Written while muted");
        assert_eq!(second["status"], "text_only");
        assert!(second.get("reason").is_none());
    }

    // The lab's bad reason is reproducible only when the client sends that reason.
    let c = Case::new(true);
    c.publish("playing", 0, "Mute stops me");
    c.report("playing", "playing", None, None);
    c.room
        .set_speech(&c.sid, false, Some("agents_muted"))
        .unwrap();
    c.report("playing", "interrupted", Some("user_interrupted"), Some(2));
    let rows = c.room.history(Some("t"));
    let row = &rows["messages"][0];
    assert_eq!(row["audio_reason"], "user_interrupted");
    assert_eq!(row["heard_chars"], 2);
}

#[test]
fn review_full_hearing_after_core_disconnect_settlement_still_counts() {
    let c = Case::new(true);
    c.publish("r", 0, "Skipped just before socket loss");
    c.room.park(&c.sid, true);
    c.room.resume(&c.sid, &[]);
    c.room
        .playback_transport(&c.sid, "r", "interrupted", None, Some(2), &Value::Null)
        .unwrap();
    c.hearing("r", "heard", None);
    assert_eq!(
        c.room.history(Some("t"))["messages"][0]["globally_heard"],
        true
    );
    assert!(c.notes().is_empty());
}

#[test]
fn review_refusal_while_only_older_turn_open_keeps_empty_obligation() {
    let c = Case::new(true);
    c.room.begin_at_focus(&c.sid, "older").unwrap();
    c.say("newer", Some("Newer words"));
    assert_eq!(
        c.publish("refused", 2, "Wait for older turn")["reason"],
        "user_speaking"
    );
    c.room
        .finish_turn(
            &c.sid,
            "older",
            None,
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(c.notes().len(), 1);
}

#[test]
fn review_expired_spoken_input_does_not_lose_attached_evidence() {
    let c = Case::new(true);
    c.publish("r", 0, "Must tell agent");
    c.report("r", "interrupted", Some("user_interrupted"), Some(2));
    c.say("next", Some("Wait"));
    {
        let mut inner = c.room.inner.lock().unwrap();
        for row in inner.journal.input_mut("t") {
            row.queued_at = 0;
        }
    }
    c.room.pending_delivery();
    assert!(
        c.pull().iter().any(|m| m["unheard"]["count"] == 1),
        "expiry must preserve owed evidence"
    );
}

#[test]
fn review_manual_replay_partial_extent_updates_history() {
    let c = Case::new(true);
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.publish("r", 0, "Missed then replayed");
    c.room.set_speech(&c.sid, true, None).unwrap();
    let history = c.room.history(Some("t"));
    let hid = history["messages"][0]["id"].as_str().unwrap();
    c.room.replay_one(&c.sid, hid, "replay").unwrap();
    c.report("replay", "interrupted", Some("user_interrupted"), Some(5));
    assert_eq!(c.room.history(Some("t"))["messages"][0]["heard_chars"], 5);
}

#[test]
fn review_manual_replay_full_ack_is_ordinary_hearing_without_skip_notice() {
    let c = Case::new(true);
    c.publish("r", 0, "Replay and skip");
    c.report("r", "heard", None, None);
    let history = c.room.history(Some("t"));
    c.room
        .replay_one(
            &c.sid,
            history["messages"][0]["id"].as_str().unwrap(),
            "replay",
        )
        .unwrap();
    c.room
        .playback_transport(&c.sid, "replay", "interrupted", None, Some(1), &Value::Null)
        .unwrap();
    c.room
        .reply_hearing(
            &c.sid,
            history["messages"][0]["id"].as_str().unwrap(),
            "replay",
            "heard",
            None,
        )
        .unwrap();
    assert!(c.notes().is_empty());
}

#[test]
fn review_full_journal_with_one_old_miss_still_accepts_current_work() {
    let mut c = Case::new(false);
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.publish("old-miss", 0, "One old unheard reply");
    c.hearing("old-miss", "not_heard", None);
    c.room.set_speech(&c.sid, true, None).unwrap();
    // The ordinary notice was acknowledged; the original remains unheard by the person.
    c.room
        .inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    let deliveries = c.room.pending_delivery();
    for (bid, rid, peer, _) in deliveries {
        c.room.settle_delivery(
            &bid,
            &rid,
            &peer.generation,
            Ok(json!({"status":"accepted"})),
        );
    }
    for n in 0..1998 {
        let uid = format!("heard-{n}");
        assert_eq!(c.room.publish(&json!({"session_id":c.sid,"thread_id":"t","revision":1,"epoch":0,"utterance_id":uid,"text":"Heard"}))["status"],"queued","filling {n}");
        c.frames("voice-reply");
        c.report(&uid, "heard", None, None);
    }
    assert_eq!(
        c.publish("fresh", 0, "Fresh work")["status"],
        "queued",
        "one old miss must not permanently block all input and publications"
    );
    assert!(
        c.room.history(Some("t"))["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["text"] == "One old unheard reply"),
        "protected miss stays visible for manual replay"
    );
}

#[test]
fn review_cancelled_turn_cannot_return_as_offline_speech() {
    let c = Case::new(false);
    let focus = c.room.select(&c.sid, "t").unwrap();
    c.room.begin_at_focus(&c.sid, "cancelled").unwrap();
    c.room
        .cancel_turn(&c.sid, "cancelled", Continuation::Standalone)
        .unwrap();
    assert!(c
        .room
        .offline_input_captured(
            &c.sid,
            "cancelled",
            "Cancelled words",
            None,
            "t",
            focus["binding"]["binding_id"].as_str().unwrap()
        )
        .is_err());
}

#[test]
fn review_unclaimed_return_note_is_reconciled_when_other_device_heard() {
    let c = Case::new(true);
    let (sender, _events) = mpsc::channel(64);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "t").unwrap();
    c.publish("r", 0, "Heard elsewhere before notice delivery");
    c.room.park(&c.sid, true);
    c.room.park(&other, true);
    c.room.resume(&c.sid, &[]);
    {
        let mut inner = c.room.inner.lock().unwrap();
        inner.unheard.note_mut("t").unwrap().due = 0;
        inner.materialize_due_notes();
    }
    c.room.resume(&other, &[]);
    c.room
        .playback(&other, "r", "heard", None, None, &Value::Null)
        .unwrap();
    assert!(
        c.pull().is_empty(),
        "a queued but unclaimed notice must not say already heard content was missed"
    );
}

#[test]
fn review_return_notice_uses_remaining_eligible_device() {
    let c = Case::new(true);
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.publish("r", 0, "Missed");
    c.hearing("r", "not_heard", None);
    c.room.set_speech(&c.sid, true, None).unwrap();
    let (sender, _events) = mpsc::channel(64);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "t").unwrap();
    c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
    c.room
        .inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    assert_eq!(
        c.pull().len(),
        1,
        "the other eligible device still constitutes a return"
    );
}

#[test]
fn review_held_publication_arriving_after_reconnect_still_notifies() {
    let c = Case::new(true);
    c.room.publish(&json!({"session_id":c.sid,"thread_id":"t","revision":c.revision(),"epoch":0,"utterance_id":"held","text":"Held across connector outage","history_only":true}));
    assert!(
        c.room.inner.lock().unwrap().unheard.note_mut("t").is_some(),
        "held publication with a present listener owes a fresh-response notice"
    );
}

#[test]
fn review_push_retry_does_not_allow_later_notice_to_overtake() {
    let c = Case::new(false);
    for n in 0..2 {
        let turn = c.room.begin_at_focus(&c.sid, &format!("turn-{n}")).unwrap();
        c.publish(&format!("refused-{n}"), turn.epoch, "Attempt");
        c.room
            .finish_turn(
                &c.sid,
                &format!("turn-{n}"),
                None,
                &Value::Null,
                Continuation::Standalone,
            )
            .unwrap();
    }
    let first = c.room.pending_delivery();
    assert_eq!(first.len(), 1);
    let (bid, rid, peer, _) = &first[0];
    c.room
        .settle_delivery(bid, rid, &peer.generation, Ok(json!({"status":"failed"})));
    let second = c.room.pending_delivery();
    assert!(
        second.is_empty(),
        "a later notice must not overtake the first notice's retry"
    );
}

fn agent_view(message: &Value) -> Value {
    let replies: Vec<Value> = message["unheard"]["replies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|reply| {
            json!({"utterance_id":reply["utterance_id"],"text":reply["text"],"fate":reply["fate"],
            "heard_chars":reply["heard_chars"]})
        })
        .collect();
    json!({"channel":message["channel"],"text":message["text"],"epoch":message["epoch"],
        "kind":message["notice"]["kind"],"instruction":message["notice"]["instruction"],
        "count":message["unheard"]["count"],"replies":replies})
}

fn matrix_messages(pull: bool, scenario: &str) -> Vec<Value> {
    let c = Case::new(pull);
    let mut previously_offered = Vec::new();
    match scenario {
        "cut_words" => {
            c.publish("r", 0, "é🐙 rest");
            c.report("r", "interrupted", Some("user_interrupted"), Some(2));
            c.say("turn", Some("Wait"));
        }
        "cut_empty_early" => {
            c.publish("r", 0, "Reply");
            c.room.begin_at_focus(&c.sid, "turn").unwrap();
            c.report("r", "interrupted", Some("user_interrupted"), Some(2));
            c.room
                .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
                .unwrap();
        }
        "cut_empty_late" => {
            c.publish("r", 0, "Reply");
            c.say("turn", None);
            c.report("r", "interrupted", Some("user_interrupted"), Some(2));
        }
        "unplayed_words" => {
            c.publish("r", 0, "Reply");
            c.report("r", "unplayed", Some("newer_turn"), None);
            c.say("turn", Some("Wait"));
        }
        "unplayed_empty_early" => {
            c.publish("r", 0, "Reply");
            c.room.begin_at_focus(&c.sid, "turn").unwrap();
            c.report("r", "unplayed", Some("newer_turn"), None);
            c.room
                .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
                .unwrap();
        }
        "unplayed_empty_late" => {
            c.publish("r", 0, "Reply");
            c.say("turn", None);
            c.report("r", "unplayed", Some("newer_turn"), None);
        }
        "refused_empty" => {
            let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
            c.publish("r", turn.epoch, "Attempt");
            c.room
                .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
                .unwrap();
        }
        "return" => {
            c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
            c.publish("r", 0, "Missed reply");
            c.hearing("r", "not_heard", None);
            c.room.set_speech(&c.sid, true, None).unwrap();
            c.room
                .inner
                .lock()
                .unwrap()
                .unheard
                .note_mut("t")
                .unwrap()
                .due = 0;
        }
        "stale" => {
            c.say("turn", Some("New words"));
            c.publish("r", 0, "Old answer");
            c.hearing("r", "not_heard", None);
        }
        "full_evidence" => {
            c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
            for n in 0..9 {
                c.publish(&format!("r{n}"), 0, &format!("{n}{}TAIL", "é".repeat(300)));
                c.hearing(&format!("r{n}"), "not_heard", None);
            }
            c.room.set_speech(&c.sid, true, None).unwrap();
            c.room
                .inner
                .lock()
                .unwrap()
                .unheard
                .note_mut("t")
                .unwrap()
                .due = 0;
        }
        "heard_correction" => {
            c.publish("r", 0, "Reply");
            c.hearing("r", "not_heard", None);
            c.say("turn", Some("Wait"));
            if pull {
                let first = c.pull();
                assert_eq!(first.len(), 1);
            } else {
                let work = c.room.pending_delivery();
                assert_eq!(work.len(), 1);
                let (bid, rid, peer, data) = &work[0];
                previously_offered.push(data.clone());
                c.room.settle_delivery(
                    bid,
                    rid,
                    &peer.generation,
                    Ok(json!({"status":"accepted"})),
                );
            }
            c.hearing("r", "heard", None);
        }
        _ => panic!("unknown matrix scenario"),
    }
    let messages: Vec<Value> = if pull {
        let first = c.pull();
        let repeated = c.pull();
        assert_eq!(
            first, repeated,
            "pull claim retries preserve IDs and payload"
        );
        first
    } else {
        let mut messages = previously_offered;
        loop {
            let work = c.room.pending_delivery();
            if work.is_empty() {
                break;
            }
            for (bid, rid, peer, data) in work {
                messages.push(data);
                c.room.settle_delivery(
                    &bid,
                    &rid,
                    &peer.generation,
                    Ok(json!({"status":"accepted"})),
                );
            }
        }
        messages
    };
    assert!(!messages.is_empty(), "{scenario} has agent-visible content");
    assert!(messages
        .iter()
        .all(|message| message["message_id"].as_str().is_some()));
    messages
        .into_iter()
        .map(|message| agent_view(&message))
        .collect()
}

#[test]
fn t16_push_pull_notice_parity_matrix() {
    for scenario in [
        "cut_words",
        "cut_empty_early",
        "cut_empty_late",
        "unplayed_words",
        "unplayed_empty_early",
        "unplayed_empty_late",
        "refused_empty",
        "return",
        "stale",
        "full_evidence",
        "heard_correction",
    ] {
        assert_eq!(
            matrix_messages(false, scenario),
            matrix_messages(true, scenario),
            "{scenario}"
        );
    }
}

#[test]
fn review_bounded_epoch_contexts_and_deferred_empty_obligations() {
    let c = Case::new(true);
    for n in 0..2047 {
        c.room.select(&c.sid, &format!("context-{n}")).unwrap();
    }
    assert_eq!(
        c.room.inner.lock().unwrap().conversations.context_count(),
        2048
    );
    assert_eq!(
        c.room.select("missing", "phantom").unwrap_err().key,
        "room.browser_absent"
    );
    assert_eq!(
        c.room.inner.lock().unwrap().conversations.context_count(),
        2048
    );
    assert_eq!(
        c.room.select(&c.sid, "overflow").unwrap_err().key,
        "room.input_backlog_full"
    );
    c.room.select(&c.sid, "t").unwrap();
    let focus = c.room.select(&c.sid, "t").unwrap();
    let focus_id = focus["binding"]["binding_id"].as_str().unwrap().to_owned();
    let turn = c.room.begin_at_focus(&c.sid, "held").unwrap();
    for n in 0..255 {
        c.room
            .send_text("Context", &c.sid, "t", &focus_id, &format!("full-{n}"))
            .unwrap();
    }
    assert_eq!(
        c.publish("refused", turn.epoch, "Attempt")["reason"],
        "user_speaking"
    );
    c.room
        .finish_turn(&c.sid, "held", None, &Value::Null, Continuation::Standalone)
        .unwrap();
    assert_eq!(
        c.room.begin_at_focus(&c.sid, "next").unwrap_err().key,
        "room.input_backlog_full"
    );
    assert_eq!(c.notes().len(), 1);
    assert!(c.room.inner.lock().unwrap().pending_notices.is_empty());
}

#[test]
fn review_terminal_push_outcomes_salvage_attached_evidence() {
    for outcome in ["unsupported", "unknown"] {
        let c = Case::new(false);
        c.publish("r", 0, "Reply evidence");
        c.report("r", "interrupted", Some("user_interrupted"), Some(2));
        c.say("turn", Some("Wait"));
        let first = c.room.pending_delivery();
        assert_eq!(first.len(), 1);
        let (bid, rid, peer, data) = &first[0];
        assert_eq!(data["unheard"]["count"], 1);
        c.room
            .settle_delivery(bid, rid, &peer.generation, Ok(json!({"status":outcome})));
        let next = c.room.pending_delivery();
        assert_eq!(next.len(), 1, "{outcome}");
        assert_eq!(next[0].3["channel"], "note");
        assert_eq!(next[0].3["unheard"]["replies"][0]["text"], "Reply evidence");
        assert_eq!(
            next[0].3["notice"]["related_message_id"],
            data["message_id"]
        );
    }
}

#[test]
fn review_channel_close_salvages_pending_attached_evidence_for_next_binding() {
    let c = Case::new(true);
    c.publish("r", 0, "Reply evidence");
    c.report("r", "interrupted", Some("user_interrupted"), Some(2));
    c.say("turn", Some("Wait"));
    let original_id = c.input("Wait")["message_id"].clone();
    c.room.close_channel("t").unwrap();
    let new = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"codex","input_mode":"pull"}),
        )
        .unwrap();
    let messages = c
        .room
        .pull_input(
            "connector",
            &json!({"binding_id":new["binding_id"],"operation":"get"}),
        )
        .unwrap();
    assert_eq!(messages["messages"][0]["channel"], "note");
    assert_eq!(
        messages["messages"][0]["notice"]["related_message_id"],
        original_id
    );
    assert_eq!(
        messages["messages"][0]["unheard"]["replies"][0]["text"],
        "Reply evidence"
    );
}

#[test]
fn review_claimed_unheard_notice_gets_correlated_heard_correction() {
    let c = Case::new(true);
    let (sender, _events) = mpsc::channel(64);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "t").unwrap();
    c.publish("r", 0, "Heard on second device");
    c.hearing("r", "not_heard", None);
    c.room
        .reply_hearing(
            &other,
            &format!("{}:voice:r", c.sid),
            "r",
            "not_heard",
            None,
        )
        .unwrap();
    c.room.park(&c.sid, true);
    c.room.park(&other, true);
    c.room.resume(&c.sid, &[]);
    c.room
        .inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    let first = c.pull();
    assert_eq!(first.len(), 1);
    let prior = first[0]["message_id"].clone();
    c.room.resume(&other, &[]);
    c.room
        .playback(&other, "r", "heard", None, None, &Value::Null)
        .unwrap();
    let messages = c.pull();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["notice"]["kind"], "reply_heard");
    assert_eq!(messages[1]["notice"]["corrects_message_id"], prior);
    assert_eq!(messages[1]["notice"]["heard"][0]["fate"], "heard");
}

#[test]
fn r5_late_original_receipt_cannot_reverse_replay_partial_or_correction_in_push_or_pull() {
    for pull in [false, true] {
        for replay_elsewhere in [false, true] {
            let c = Case::new(pull);
            c.publish("original", 0, "abcdefghij");
            c.report("original", "interrupted", Some("user_interrupted"), Some(2));
            c.say("wait", Some("Wait"));
            let initial = if pull {
                c.pull()
                    .into_iter()
                    .find(|message| message["text"] == "Wait")
                    .unwrap()
            } else {
                let work = c.room.pending_delivery();
                assert_eq!(work.len(), 1);
                let (bid, rid, peer, data) = &work[0];
                c.room.settle_delivery(
                    bid,
                    rid,
                    &peer.generation,
                    Ok(json!({"status":"accepted"})),
                );
                data.clone()
            };
            assert_eq!(
                initial["unheard"]["replies"][0]["heard_chars"], 2,
                "pull={pull} elsewhere={replay_elsewhere}: {initial}"
            );
            let original_id = initial["message_id"].clone();
            let history_id = c.room.history(Some("t"))["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["role"] == "assistant")
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let mut other_events = None;
            let other = if replay_elsewhere {
                let (sender, events) = mpsc::channel(64);
                other_events = Some(events);
                let sid = c.room.join("other".into(), "en".into(), sender).unwrap();
                c.room.select(&sid, "t").unwrap();
                sid
            } else {
                c.sid.clone()
            };
            let _keep_other_channel_open = &other_events;
            c.room
                .replay_one(&other, &history_id, "replay-eight")
                .unwrap();
            c.room
                .playback(
                    &other,
                    "replay-eight",
                    "interrupted",
                    Some("user_interrupted"),
                    Some(8),
                    &Value::Null,
                )
                .unwrap();
            let correction = c
                .notes()
                .into_iter()
                .find(|note| note["notice"]["kind"] == "reply_heard")
                .unwrap();
            assert_eq!(correction["notice"]["corrects_message_id"], original_id);
            assert_eq!(correction["notice"]["heard"][0]["fate"], "partial");
            assert_eq!(correction["notice"]["heard"][0]["heard_chars"], 8);
            c.report("original", "interrupted", Some("user_interrupted"), Some(4));
            let rows = c.room.history(Some("t"));
            let row = rows["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == history_id)
                .unwrap();
            assert_eq!(
                row["heard_chars"], 8,
                "pull={pull} elsewhere={replay_elsewhere}"
            );
            let corrections: Vec<_> = c
                .notes()
                .into_iter()
                .filter(|note| note["notice"]["kind"] == "reply_heard")
                .collect();
            assert_eq!(
                corrections.len(),
                1,
                "pull={pull} elsewhere={replay_elsewhere}"
            );
            assert_eq!(corrections[0]["notice"]["heard"][0]["heard_chars"], 8);
            // A smaller fact must not contradict an already exposed correction.
            {
                let mut inner = c.room.inner.lock().unwrap();
                inner.journal.find_mut(&history_id).unwrap().heard_chars = Some(4);
                inner.reconcile_unclaimed("t");
            }
            assert_eq!(
                c.notes()
                    .iter()
                    .filter(|note| note["notice"]["kind"] == "reply_heard")
                    .count(),
                1
            );
        }
    }
}

#[test]
fn r6_old_miss_heard_on_other_device_resolves_after_leaving_recent_history() {
    use crate::control::room::journal::Row;
    let c = Case::new(true);
    c.publish("old", 0, "Older missed reply");
    c.report("old", "interrupted", Some("user_interrupted"), Some(2));
    let history_id = c.room.history(Some("t"))["messages"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    {
        let mut inner = c.room.inner.lock().unwrap();
        for n in 0..1001 {
            inner.append_row(Row {
                id: format!("later-{n}"),
                thread: "t".into(),
                role: "user",
                text: "later".into(),
                status: "read".into(),
                ..Row::default()
            });
        }
    }
    assert!(c.room.history(Some("t"))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == history_id));
    let (sender, _events) = mpsc::channel(64);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "t").unwrap();
    c.room
        .replay_one(&other, &history_id, "other-replay")
        .unwrap();
    c.room
        .playback(&other, "other-replay", "heard", None, None, &Value::Null)
        .unwrap();
    assert!(!c.room.history(Some("t"))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == history_id));
    let resolved = c
        .room
        .history_resolving(Some("t"), std::slice::from_ref(&history_id));
    assert_eq!(resolved["resolved"][0]["id"], history_id);
    assert_eq!(resolved["resolved"][0]["globally_heard"], true);
    assert_eq!(resolved["resolved"][0]["heard_chars"], 18);
    assert_eq!(resolved["missing_ids"], json!([]));
}

#[test]
fn review_finished_merged_transcript_keeps_its_words_with_another_turn_open() {
    let c = Case::new(true);
    c.room.begin_at_focus(&c.sid, "first").unwrap();
    c.room.begin_at_focus(&c.sid, "later").unwrap();
    c.room
        .finish_turn(
            &c.sid,
            "first",
            Some("Merged words"),
            &Value::Null,
            Continuation::Merged,
        )
        .unwrap();
    assert_eq!(c.input("Merged words")["text"], "Merged words");
}

#[test]
fn t16_shared_voice_to_core_protocol_fixtures_have_the_documented_fates() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../docs/fixtures/conversation-flow.json"
    ))
    .unwrap();
    for (index, mapping) in fixture["playback"].as_array().unwrap().iter().enumerate() {
        let c = Case::new(true);
        c.publish("r", 0, fixture["text"].as_str().unwrap());
        let receipt = &mapping["core"];
        c.room
            .playback_transport(
                &c.sid,
                "r",
                receipt["status"].as_str().unwrap(),
                receipt["reason"].as_str(),
                receipt["heard_chars"].as_u64(),
                &Value::Null,
            )
            .unwrap();
        let row = &c.room.history(Some("t"))["messages"][0];
        for field in ["status", "audio_reason"] {
            assert_eq!(
                row[field], mapping["history"][field],
                "fixture {index} {field}"
            );
        }
        assert_eq!(row["hearing"], Value::Null, "fixture {index} hearing");
    }
    for mapping in fixture["hearing"].as_array().unwrap() {
        let c = Case::new(true);
        if mapping["publication_status"] == "text_only" {
            c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
        }
        c.publish("r", 0, fixture["text"].as_str().unwrap());
        let data = &mapping["frame"]["data"];
        c.hearing(
            "r",
            data["outcome"].as_str().unwrap(),
            data["heard_chars"].as_u64(),
        );
        let row = &c.room.history(Some("t"))["messages"][0];
        for field in [
            "hearing",
            "heard_chars",
            "globally_heard",
            "hearing_unconfirmed",
        ] {
            assert_eq!(row[field], mapping["history"][field], "{field}");
        }
    }
    let turns = fixture["turns"].as_array().unwrap();
    assert_eq!(turns[0]["core"]["data"]["phase"], "cancelled");
    assert_eq!(turns[1]["core"]["data"]["phase"], "finished");
    assert_eq!(turns[2]["core"]["data"]["offline"], true);
    assert!(turns
        .iter()
        .all(|turn| turn["core"]["data"]["client_msg_id"].as_str().is_some()));
}

#[test]
fn t16_unacknowledged_notice_crosses_pull_push_handover_with_one_message_id() {
    let c = Case::new(true);
    let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
    c.publish("refused", turn.epoch, "Attempt");
    c.room
        .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
        .unwrap();
    let claimed = c.pull();
    let id = claimed[0]["message_id"].clone();
    let push = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"codex","input_mode":"push"}),
        )
        .unwrap();
    let due = c.room.pending_delivery();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].3["message_id"], id);
    assert_eq!(due[0].3["notice"], claimed[0]["notice"]);
    let (bid, rid, peer, _) = &due[0];
    assert_eq!(push["binding_id"], *bid);
    c.room
        .settle_delivery(bid, rid, &peer.generation, Ok(json!({"status":"failed"})));
    let pull = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"codex","input_mode":"pull"}),
        )
        .unwrap();
    let fetched = c
        .room
        .pull_input(
            "connector",
            &json!({"binding_id":pull["binding_id"],"operation":"get"}),
        )
        .unwrap();
    assert_eq!(fetched["messages"][0]["message_id"], id);
    assert_eq!(fetched["messages"][0]["notice"], claimed[0]["notice"]);
}

#[test]
fn review_full_ack_after_local_stop_and_disconnect_orders_agree() {
    for status in ["interrupted", "unplayed"] {
        for before_disconnect in [false, true] {
            let c = Case::new(true);
            c.publish("r", 0, "Skip me");
            if before_disconnect {
                c.room
                    .playback_transport(&c.sid, "r", status, None, Some(0), &Value::Null)
                    .unwrap();
                c.hearing("r", "heard", None);
            }
            c.room.park(&c.sid, true);
            c.room.resume(&c.sid, &[]);
            if !before_disconnect {
                c.room
                    .playback_transport(&c.sid, "r", status, None, Some(0), &Value::Null)
                    .unwrap();
                c.hearing("r", "heard", None);
            }
            c.hearing("r", "heard", None);
            assert_eq!(
                c.room.history(Some("t"))["messages"][0]["globally_heard"],
                true
            );
            assert!(
                c.notes().is_empty(),
                "{status} before_disconnect={before_disconnect}"
            );
        }
    }
}

#[test]
fn review_replay_partial_extent_follows_words_or_empty_terminal() {
    for words in [false, true] {
        let c = Case::new(true);
        c.room.set_speech(&c.sid, false, Some("voice_off")).unwrap();
        c.publish("r", 0, "Missed then replayed");
        c.room.set_speech(&c.sid, true, None).unwrap();
        let history_id = c.room.history(Some("t"))["messages"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        c.room.replay_one(&c.sid, &history_id, "replay").unwrap();
        c.room.begin_at_focus(&c.sid, "turn").unwrap();
        c.report("replay", "interrupted", Some("user_interrupted"), Some(5));
        c.room
            .finish_turn(
                &c.sid,
                "turn",
                words.then_some("Wait"),
                &Value::Null,
                Continuation::Standalone,
            )
            .unwrap();
        let messages = c.pull();
        let with_evidence = messages
            .iter()
            .find(|message| message["unheard"]["count"] == 1)
            .unwrap();
        assert_eq!(with_evidence["unheard"]["replies"][0]["heard_chars"], 5);
        assert_eq!(
            with_evidence["channel"],
            if words { "voice" } else { "note" }
        );
    }
}

#[test]
fn review_claimed_notice_gets_full_hearing_and_replay_corrections() {
    for replay in [false, true] {
        let c = Case::new(true);
        c.publish("r", 0, "Reply");
        c.hearing("r", "not_heard", None);
        c.room.park(&c.sid, true);
        c.room.resume(&c.sid, &[]);
        c.room
            .inner
            .lock()
            .unwrap()
            .unheard
            .note_mut("t")
            .unwrap()
            .due = 0;
        let prior = c.pull()[0]["message_id"].clone();
        if replay {
            let history_id = c.room.history(Some("t"))["messages"][0]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            c.room.replay_one(&c.sid, &history_id, "replay").unwrap();
            c.report("replay", "heard", None, None);
        } else {
            c.hearing("r", "heard", None);
        }
        let messages = c.pull();
        let correction = messages
            .iter()
            .find(|message| message["notice"]["kind"] == "reply_heard")
            .unwrap();
        assert_eq!(correction["notice"]["corrects_message_id"], prior);
        assert_eq!(correction["notice"]["heard"][0]["fate"], "heard");
    }
}

#[test]
fn review_claim_released_after_ttl_keeps_attached_evidence_and_id() {
    let c = Case::new(true);
    c.publish("r", 0, "Reply");
    c.report("r", "interrupted", Some("user_interrupted"), Some(2));
    c.say("turn", Some("Wait"));
    let claimed = c.pull();
    let id = claimed[0]["message_id"].clone();
    {
        let mut inner = c.room.inner.lock().unwrap();
        for row in inner.journal.input_mut("t") {
            row.queued_at = 0;
        }
    }
    let replacement = c
        .room
        .register(
            "connector",
            &json!({"thread":"t","harness":"codex",
        "input_mode":"pull","binding_id":"replacement"}),
        )
        .unwrap();
    let fetched = c
        .room
        .pull_input(
            "connector",
            &json!({"binding_id":replacement["binding_id"],"operation":"get"}),
        )
        .unwrap();
    assert_eq!(fetched["messages"][0]["message_id"], id);
    assert_eq!(fetched["messages"][0]["unheard"], claimed[0]["unheard"]);
}

#[test]
fn review_duplicate_cut_receipt_has_no_second_note_in_push_or_pull() {
    for pull in [false, true] {
        let c = Case::new(pull);
        c.publish("r", 0, "Partly heard");
        c.report("r", "interrupted", Some("user_interrupted"), Some(3));
        c.say("turn", Some("Wait"));
        let original = c.input("Wait")["message_id"].clone();
        c.report("r", "interrupted", Some("user_interrupted"), Some(3));
        assert!(c.notes().is_empty());
        let messages = if pull {
            c.pull()
        } else {
            c.room
                .pending_delivery()
                .into_iter()
                .map(|(_, _, _, data)| data)
                .collect()
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["message_id"], original);
    }
}

#[test]
fn review_offline_epoch_withdraws_second_device_and_correlates_its_late_receipt() {
    let mut c = Case::new(true);
    let (sender, mut other_events) = mpsc::channel(64);
    let other = c.room.join("other".into(), "en".into(), sender).unwrap();
    c.room.select(&other, "t").unwrap();
    c.publish("r", 0, "Old answer");
    c.frames("voice-reply");
    while other_events.try_recv().is_ok() {}
    let focus = c.room.select(&c.sid, "t").unwrap();
    let binding = focus["binding"]["binding_id"].as_str().unwrap();
    c.room
        .offline_input_captured(&c.sid, "offline", "Offline words", None, "t", binding)
        .unwrap();
    assert_eq!(c.frames("voice-reply-withdrawn").len(), 1);
    assert!(std::iter::from_fn(|| other_events.try_recv().ok())
        .any(|frame| frame["type"] == "voice-reply-withdrawn"));
    c.room.park(&c.sid, true);
    c.hearing("r", "not_heard", None);
    c.room
        .playback(
            &other,
            "r",
            "unplayed",
            Some("newer_turn"),
            None,
            &Value::Null,
        )
        .unwrap();
    let messages = c.pull();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["notice"]["kind"], "reply_not_heard");
    assert_eq!(
        messages[1]["notice"]["related_message_id"],
        messages[0]["message_id"]
    );
    assert_eq!(messages[1]["unheard"]["replies"][0]["text"], "Old answer");
}

#[test]
fn review_refusal_obligation_survives_both_turn_orders_and_devices() {
    for two_devices in [false, true] {
        for reverse in [false, true] {
            for words in [false, true] {
                let c = Case::new(true);
                let second = if two_devices {
                    let (sender, _events) = mpsc::channel(64);
                    let sid = c.room.join("other".into(), "en".into(), sender).unwrap();
                    c.room.select(&sid, "t").unwrap();
                    sid
                } else {
                    c.sid.clone()
                };
                c.room.begin_at_focus(&c.sid, "first").unwrap();
                let latest = c.room.begin_at_focus(&second, "second").unwrap();
                assert_eq!(
                    c.publish("refused", latest.epoch, "Attempt")["reason"],
                    "user_speaking"
                );
                let turns = if reverse {
                    [(&second, "second"), (&c.sid, "first")]
                } else {
                    [(&c.sid, "first"), (&second, "second")]
                };
                for (index, (sid, turn)) in turns.into_iter().enumerate() {
                    c.room
                        .finish_turn(
                            sid,
                            turn,
                            (words && index == 0).then_some("Words"),
                            &Value::Null,
                            Continuation::Standalone,
                        )
                        .unwrap();
                }
                assert_eq!(
                    c.notes().len(),
                    usize::from(!words),
                    "two_devices={two_devices} reverse={reverse} words={words}"
                );
            }
        }
    }
}

fn acknowledge_one_untold_input(c: &Case, pull: bool) -> Value {
    if pull {
        let messages = c.pull();
        assert_eq!(messages.len(), 1, "{messages:?}");
        let delivered = messages[0].clone();
        let ack = c
            .room
            .pull_input(
                "connector",
                &json!({"binding_id":c.bid,"operation":"get",
                    "ack_ids":[delivered["message_id"]]}),
            )
            .unwrap();
        assert_eq!(ack["acknowledged"], 1);
        assert!(c.pull().is_empty());
        delivered
    } else {
        let work = c.room.pending_delivery();
        assert_eq!(work.len(), 1);
        let (bid, rid, peer, delivered) = &work[0];
        c.room
            .settle_delivery(bid, rid, &peer.generation, Ok(json!({"status":"accepted"})));
        assert!(c.room.pending_delivery().is_empty());
        delivered.clone()
    }
}

#[test]
fn h1_unrelated_late_receipt_keeps_untold_not_heard_context_in_words_and_note() {
    for pull in [false, true] {
        for standalone_note in [false, true] {
            let c = Case::new(pull);
            let (sender, _events) = mpsc::channel(64);
            let second = c.room.join("other".into(), "en".into(), sender).unwrap();
            c.room.select(&second, "t").unwrap();
            c.publish("a", 0, "A was not heard");
            c.publish("b", 0, "B was heard later");
            c.hearing("a", "not_heard", None);
            c.room
                .reply_hearing(
                    &second,
                    &format!("{}:voice:a", c.sid),
                    "a",
                    "not_heard",
                    None,
                )
                .unwrap();
            if standalone_note {
                c.room.park(&c.sid, true);
                c.room.resume(&c.sid, &[]);
                let mut inner = c.room.inner.lock().unwrap();
                inner.unheard.note_mut("t").unwrap().due = 0;
                inner.materialize_due_notes();
                assert_eq!(inner.journal.waiting_input().0, 1);
            } else {
                c.say("turn", Some("Wait"));
            }
            c.room
                .reply_hearing(&second, &format!("{}:voice:b", c.sid), "b", "heard", None)
                .unwrap();
            let delivered = acknowledge_one_untold_input(&c, pull);
            assert_eq!(delivered["text"], if standalone_note { "" } else { "Wait" });
            assert_eq!(delivered["unheard"]["count"], 1);
            assert_eq!(
                delivered["unheard"]["replies"][0]["text"],
                "A was not heard"
            );
            assert_eq!(delivered["unheard"]["replies"][0]["fate"], "not_heard");
            assert_eq!(
                c.room.history(Some("t"))["messages"][0]["hearing"],
                "not_heard"
            );
        }
    }
}

#[test]
fn h5_full_receipt_removes_miss_but_keeps_unexposed_empty_refusal_obligation() {
    for pull in [false, true] {
        let c = Case::new(pull);
        let (sender, _events) = mpsc::channel(64);
        let second = c.room.join("other".into(), "en".into(), sender).unwrap();
        c.room.select(&second, "t").unwrap();
        c.publish("a", 0, "Earlier reply");
        c.hearing("a", "partial", Some(2));
        let turn = c.room.begin_at_focus(&c.sid, "turn").unwrap();
        assert_eq!(
            c.publish("refused", turn.epoch, "Await words")["reason"],
            "user_speaking"
        );
        c.room
            .finish_turn(&c.sid, "turn", None, &Value::Null, Continuation::Standalone)
            .unwrap();
        let before = c.notes();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0]["notice"]["kind"], "turn_empty");
        assert_eq!(before[0]["unheard"]["replies"][0]["fate"], "partial");
        c.room
            .reply_hearing(&second, &format!("{}:voice:a", c.sid), "a", "heard", None)
            .unwrap();
        let delivered = acknowledge_one_untold_input(&c, pull);
        assert_eq!(delivered["message_id"], before[0]["message_id"]);
        assert_eq!(delivered["notice"]["kind"], "turn_empty");
        assert!(delivered.get("unheard").is_none());

        let miss_only = Case::new(pull);
        let (sender, _events) = mpsc::channel(64);
        let second = miss_only
            .room
            .join("other".into(), "en".into(), sender)
            .unwrap();
        miss_only.room.select(&second, "t").unwrap();
        miss_only.publish("a", 0, "Ordinary miss");
        miss_only.hearing("a", "partial", Some(2));
        miss_only.room.park(&miss_only.sid, true);
        miss_only.room.resume(&miss_only.sid, &[]);
        {
            let mut inner = miss_only.room.inner.lock().unwrap();
            inner.unheard.note_mut("t").unwrap().due = 0;
            inner.materialize_due_notes();
        }
        miss_only
            .room
            .reply_hearing(
                &second,
                &format!("{}:voice:a", miss_only.sid),
                "a",
                "heard",
                None,
            )
            .unwrap();
        if pull {
            assert!(miss_only.pull().is_empty());
        } else {
            assert!(miss_only.room.pending_delivery().is_empty());
        }
    }
}

/// Say "hello" and let it wait past the delivery deadline, as for an agent that never fetches it.
fn expire_input(c: &Case) {
    let mut inner = c.room.inner.lock().unwrap();
    for row in inner.journal.input_mut("t") {
        row.queued_at = 0;
    }
    drop(inner);
    c.room.pending_delivery();
}

#[test]
fn input_past_the_deadline_is_not_sent_and_the_agent_speaks_again() {
    let c = Case::new(true);
    let epoch = c.say("turn", Some("hello"));
    assert_eq!(c.publish("early", 0, "Too soon")["status"], "text_only");
    expire_input(&c);
    assert_eq!(
        c.room.history(Some("t"))["messages"][0]["status"],
        "not_sent"
    );
    let answer = c.publish("late", 0, "Still here");
    assert_ne!(answer["status"], "text_only", "{answer}");
    assert_eq!(answer["epoch"], epoch);
}

#[test]
fn input_delivered_within_the_deadline_keeps_the_newer_turn_gate() {
    let c = Case::new(true);
    c.say("turn", Some("hello"));
    assert_eq!(c.pull().len(), 1);
    expire_input(&c);
    assert_eq!(c.publish("old", 0, "Old answer")["status"], "text_only");
}

#[test]
fn a_late_pull_of_expired_input_delivers_nothing() {
    let c = Case::new(true);
    c.say("turn", Some("hello"));
    expire_input(&c);
    assert!(c.pull().is_empty());
    assert_eq!(
        c.room.history(Some("t"))["messages"][0]["status"],
        "not_sent"
    );
}
