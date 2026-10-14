//! Task T core acceptance cases. Browser and native host.voice share these room frames.
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::pull_room;
use crate::control::room::Room;

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
    fn say(&self, id: &str, text: Option<&str>) -> u64 {
        let turn = self.room.begin_turn(&self.sid, id).unwrap();
        self.room
            .finish_turn(&self.sid, id, text, &Value::Null)
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
fn t01_playing_reply_cut_carries_full_unicode_evidence_with_the_words() {
    let c = Case::new(false);
    assert_eq!(c.publish("r", 0, "é🐙 rest")["status"], "queued");
    c.report("r", "playing", None, None);
    c.report("r", "interrupted", Some("user_interrupted"), Some(2));
    assert_eq!(c.say("turn", Some("Wait")), 1);
    let payload = c.input("Wait");
    assert_eq!(payload["notice"]["kind"], "reply_interrupted");
    let reply = &payload["unheard"]["replies"][0];
    assert_eq!(
        (&reply["text"], &reply["heard_chars"], &reply["fate"]),
        (&json!("é🐙 rest"), &json!(2), &json!("interrupted"))
    );
}

#[test]
fn t02a_accepted_before_start_is_withdrawn_and_attached_to_words() {
    let mut c = Case::new(false);
    assert_eq!(c.publish("r", 0, "Not yet started")["status"], "queued");
    c.frames("voice-reply");
    let turn = c.room.begin_turn(&c.sid, "turn").unwrap();
    assert_eq!(turn.epoch, 1);
    assert_eq!(c.frames("voice-reply-withdrawn").len(), 1);
    c.report("r", "unplayed", Some("newer_turn"), None);
    c.room
        .finish_turn(&c.sid, "turn", Some("Actually"), &Value::Null)
        .unwrap();
    assert_eq!(
        c.input("Actually")["unheard"]["replies"][0]["reason"],
        "newer_turn"
    );
    assert!(c.frames("voice-reply").is_empty());
}

#[test]
fn t02b_accepted_before_start_with_empty_turn_queues_empty_notice() {
    let c = Case::new(true);
    c.publish("r", 0, "Not played");
    c.room.begin_turn(&c.sid, "turn").unwrap();
    c.report("r", "unplayed", Some("newer_turn"), None);
    c.room
        .finish_turn(&c.sid, "turn", None, &Value::Null)
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
    let turn = c.room.begin_turn(&c.sid, "turn").unwrap();
    let refused = c.publish("r", turn.epoch, "Attempt");
    assert_eq!(refused["reason"], "user_speaking");
    c.room
        .finish_turn(&c.sid, "turn", Some("Words"), &Value::Null)
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
    let turn = c.room.begin_turn(&c.sid, "turn").unwrap();
    c.publish("r", turn.epoch, "Attempt");
    c.room
        .finish_turn(&c.sid, "turn", None, &Value::Null)
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
    assert_eq!(
        (&answer["status"], &answer["reason"]),
        (&json!("text_only"), &json!("newer_turn"))
    );
    assert!(c.frames("voice-reply").is_empty());
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
    c.room.begin_turn(&c.sid, "turn").unwrap();
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
    assert_eq!(answer["reason"], reason);
    c.room.set_speech(&c.sid, true, None).unwrap();
    assert!(c.frames("voice-reply").is_empty());
    let history = c.room.history(Some("t"));
    assert_eq!(history["messages"][0]["heard_chars"], 3);
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
    let turn = c.room.begin_turn(&c.sid, "turn").unwrap();
    c.room.select(&c.sid, "other").unwrap();
    c.room
        .finish_turn(&c.sid, "turn", Some("Original"), &Value::Null)
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
    let turn = c.room.begin_turn(&c.sid, "turn").unwrap();
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
        .finish_turn(&c.sid, "turn", Some("Spoken"), &Value::Null)
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
    let second = c.room.begin_turn(&c.sid, "mic-muted").unwrap();
    assert_eq!(second.epoch, 1);
    c.room
        .finish_turn(&c.sid, "mic-muted", None, &Value::Null)
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
    let a = c.room.begin_turn(&c.sid, "a").unwrap();
    let b = c.room.begin_turn(&other, "b").unwrap();
    assert_eq!((a.epoch, b.epoch), (1, 1));
    let duplicate = c.room.begin_turn(&c.sid, "a").unwrap();
    assert_eq!(duplicate.epoch, 1);
    c.room
        .finish_turn(&other, "b", Some("Other thread"), &Value::Null)
        .unwrap();
    c.room
        .finish_turn(&c.sid, "a", Some("This thread"), &Value::Null)
        .unwrap();
    assert_eq!(c.input("This thread")["epoch"], 1);
    assert_eq!(c.room.history(Some("other"))["messages"][0]["epoch"], 1);
}

#[test]
fn t12_skip_is_heard_by_choice_and_does_not_stale_the_next_reply() {
    let mut c = Case::new(true);
    c.publish("first", 0, "Skip this");
    c.publish("next", 0, "Play this");
    c.frames("voice-reply");
    c.report("first", "interrupted", Some("user_skipped"), Some(2));
    let history = c.room.history(Some("t"));
    assert_eq!(history["messages"][0]["status"], "skipped");
    assert_eq!(history["messages"][0]["globally_heard"], true);
    assert_eq!(history["messages"][1]["status"], "queued");
    assert_eq!(c.notes()[0]["notice"]["kind"], "reply_skipped");
    assert!(c.notes()[0].get("unheard").is_none());
    assert_eq!(c.pull()[0]["notice"]["skipped"][0]["text"], "Skip this");
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
        let turn = c.room.begin_turn(&c.sid, "turn").unwrap();
        c.publish("refused", turn.epoch, "Attempt");
        c.room
            .finish_turn(&c.sid, "turn", None, &Value::Null)
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
    assert_eq!(
        (&first["status"], &first["reason"]),
        (&json!("text_only"), &json!("connector_disconnected"))
    );
    request["history_only"] = json!(false);
    assert_eq!(c.room.publish(&request)["error"], "room.speech_conflict");
    assert!(c.frames("voice-reply").is_empty());
}

#[test]
fn t11_merged_turn_transfers_the_empty_obligation_to_its_successor() {
    let c = Case::new(true);
    let first = c.room.begin_turn(&c.sid, "first").unwrap();
    c.publish("refused", first.epoch, "Attempt");
    let second = c.room.begin_turn(&c.sid, "second").unwrap();
    c.room
        .finish_turn_merged(&c.sid, "first", None, &Value::Null, true)
        .unwrap();
    assert!(c.notes().is_empty());
    c.room
        .finish_turn(&c.sid, "second", None, &Value::Null)
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
    let turn = c.room.begin_turn(&c.sid, "turn").unwrap();
    c.publish("refused", turn.epoch, "Attempt");
    c.room
        .finish_turn(&c.sid, "turn", None, &Value::Null)
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
    assert_eq!(notes[0]["notice"]["kind"], "reply_not_played");
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
    let first = c.room.begin_turn(&c.sid, "first").unwrap();
    let other = c.room.begin_turn(&second, "other").unwrap();
    c.room
        .finish_turn(&c.sid, "first", Some("First"), &Value::Null)
        .unwrap();
    assert_eq!(
        c.publish("attempt", other.epoch, "Still open")["reason"],
        "user_speaking"
    );
    c.room
        .finish_turn(&second, "other", None, &Value::Null)
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
