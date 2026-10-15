//! Turns waiting for their words: a newer turn, a change of focus or a turn spoken offline never loses or misplaces
//! the words of a turn already started, and an offline message is ordered after every turn before it.
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::pull_room;
use super::support::BeginAtFocus;
use crate::control::room::{Continuation, Room};

/// A room with conversations `x` and `y`, and a call focused on `x`.
fn room_with_call() -> (tempfile::TempDir, Room, String, mpsc::Receiver<Value>) {
    let (directory, room, _) = pull_room(&["connector"]);
    for thread in ["x", "y"] {
        room.register("connector", &json!({"thread":thread,"harness":"claude"}))
            .unwrap();
    }
    let (events, received) = mpsc::channel(256);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "x").unwrap();
    (directory, room, sid, received)
}

/// The words the person sent to `thread`, oldest first.
fn said(room: &Room, thread: &str) -> Vec<String> {
    room.history(Some(thread))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| message["text"].as_str().unwrap().to_owned())
        .collect()
}

/// The conversation of each message due for delivery.
fn delivered_to(room: &Room) -> Vec<String> {
    room.pending_delivery()
        .into_iter()
        .map(|(_, _, _, data)| data["thread"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn delayed_captured_start_routes_to_original_focus_after_http_selection_and_binding_replacement() {
    let (_directory, room, sid, _events) = room_with_call();
    let first = room.snapshot(Some(&sid))["binding"].clone();
    room.select(&sid, "y").unwrap();
    room.select(&sid, "x").unwrap();
    let second = room.snapshot(Some(&sid))["binding"].clone();
    assert_ne!(first["binding_id"], second["binding_id"]);
    room.select(&sid, "y").unwrap();
    let turn = room
        .begin_turn(&sid, "delayed", "x", first["binding_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(turn.thread_id.as_deref(), Some("x"));
    assert_eq!(turn.epoch, 1);
    assert_eq!(
        room.snapshot(Some(&sid))["call"]["target"]["thread_id"],
        "y"
    );
    assert_eq!(
        room.begin_turn(&sid, "delayed", "y", first["binding_id"].as_str().unwrap())
            .unwrap_err()
            .key,
        "room.message_conflict"
    );
    room.finish_turn(
        &sid,
        "delayed",
        Some("A words"),
        &Value::Null,
        Continuation::Standalone,
    )
    .unwrap();
    assert_eq!(said(&room, "x"), ["A words"]);
    assert!(said(&room, "y").is_empty());
    assert_eq!(
        room.history(Some("y"))["messages"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        room.begin_turn(&sid, "invalid", "x", "unknown")
            .unwrap_err()
            .key,
        "room.focus_changed"
    );
}

#[test]
fn explicit_history_resolution_reports_old_heard_row_and_missing_identity() {
    use crate::control::room::journal::Row;
    let (_directory, room, _sid, _events) = room_with_call();
    {
        let mut inner = room.inner.lock().unwrap();
        inner.append_row(Row {
            id: "older-miss".into(),
            thread: "x".into(),
            role: "assistant",
            text: "old missed reply".into(),
            status: "interrupted".into(),
            reason: Some("user_interrupted".into()),
            hearing: Some("not_heard".into()),
            heard_chars: Some(0),
            ..Row::default()
        });
        for n in 0..1001 {
            inner.append_row(Row {
                id: format!("new-{n}"),
                thread: "x".into(),
                role: "user",
                text: "new".into(),
                ..Row::default()
            });
        }
    }
    assert!(room.history(Some("x"))["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["id"] == "older-miss"));
    {
        let mut inner = room.inner.lock().unwrap();
        let row = inner.journal.find_mut("older-miss").unwrap();
        row.globally_heard = true;
        row.hearing = Some("heard".into());
        row.heard_chars = Some(16);
    }
    let ordinary = room.history(Some("x"));
    assert!(!ordinary["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["id"] == "older-miss"));
    let resolved = room.history_resolving(Some("x"), &["older-miss".into(), "gone".into()]);
    assert_eq!(resolved["resolved"][0]["id"], "older-miss");
    assert_eq!(resolved["resolved"][0]["globally_heard"], true);
    assert_eq!(resolved["resolved"][0]["heard_chars"], 16);
    assert_eq!(resolved["missing_ids"], json!(["gone"]));
    assert_eq!(
        room.history_resolving(Some("y"), &["older-miss".into()])["missing_ids"],
        json!(["older-miss"])
    );
}

fn publish(room: &Room, sid: &str, uid: &str, revision: u64) -> Value {
    let epoch = revision
        .saturating_sub(1)
        .min(room.inner.lock().unwrap().conversations.epoch("x"));
    room.publish(
        &json!({"session_id":sid,"thread_id":"x","revision":revision,
            "epoch":epoch,
            "utterance_id":uid,"text":"A reply."}),
    )
}

#[test]
fn a_newer_turn_does_not_lose_the_words_of_one_still_transcribed() {
    let (_directory, room, sid, _events) = room_with_call();
    let a = room.begin_at_focus(&sid, "a").unwrap();
    let b = room.begin_at_focus(&sid, "b").unwrap();
    room.finish_turn(
        &sid,
        &a.turn_id,
        Some("First words"),
        &Value::Null,
        Continuation::Standalone,
    )
    .unwrap();
    room.finish_turn(
        &sid,
        &b.turn_id,
        Some("Second words"),
        &Value::Null,
        Continuation::Standalone,
    )
    .unwrap();
    assert_eq!(said(&room, "x"), ["First words", "Second words"]);
    // Each turn ends once.
    assert_eq!(
        room.finish_turn(
            &sid,
            &a.turn_id,
            Some("First words"),
            &Value::Null,
            Continuation::Standalone
        )
        .unwrap_err()
        .key,
        "room.input_ended"
    );
}

#[test]
fn a_turn_finished_after_the_focus_moved_goes_once_to_the_conversation_it_was_spoken_to() {
    let (_directory, room, sid, _events) = room_with_call();
    let a = room.begin_at_focus(&sid, "a").unwrap();
    room.select(&sid, "y").unwrap();
    room.finish_turn(
        &sid,
        &a.turn_id,
        Some("Words for x"),
        &Value::Null,
        Continuation::Standalone,
    )
    .unwrap();
    assert_eq!(said(&room, "x"), ["Words for x"]);
    assert!(said(&room, "y").is_empty());
    assert_eq!(delivered_to(&room), ["x"]);
}

#[test]
fn a_cancelled_turn_drops_its_words_and_leaves_the_others() {
    let (_directory, room, sid, _events) = room_with_call();
    let a = room.begin_at_focus(&sid, "a").unwrap();
    let b = room.begin_at_focus(&sid, "b").unwrap();
    room.cancel_turn(&sid, &a.turn_id, Continuation::Standalone)
        .unwrap();
    assert_eq!(
        room.finish_turn(
            &sid,
            &a.turn_id,
            Some("Dropped"),
            &Value::Null,
            Continuation::Standalone
        )
        .unwrap_err()
        .key,
        "room.input_ended"
    );
    room.finish_turn(
        &sid,
        &b.turn_id,
        Some("Kept"),
        &Value::Null,
        Continuation::Standalone,
    )
    .unwrap();
    assert_eq!(said(&room, "x"), ["Kept"]);
}

#[test]
fn an_offline_message_is_newer_than_the_turn_before_it() {
    let (_directory, room, sid, _events) = room_with_call();
    let live = room.begin_at_focus(&sid, "live").unwrap();
    room.finish_turn(
        &sid,
        &live.turn_id,
        Some("Live words"),
        &Value::Null,
        Continuation::Standalone,
    )
    .unwrap();
    let offline = room
        .offline_input(&sid, "m1", "Offline words", None)
        .unwrap();
    let offline_revision = offline["revision"].as_u64().unwrap();
    assert!(offline_revision > live.revision);
    // The reply to the offline message is spoken; a late reply to the live turn before it is not.
    assert_eq!(
        publish(&room, &sid, "to-offline", offline_revision)["status"],
        "queued"
    );
    let late = publish(&room, &sid, "to-live", live.revision);
    assert_eq!(late["status"], "text_only");
    assert_eq!(late["status"], "text_only");
    assert!(late.get("reason").is_none());
}

#[test]
fn of_two_offline_messages_a_late_reply_to_the_first_is_not_spoken() {
    let (_directory, room, sid, _events) = room_with_call();
    let first = room.offline_input(&sid, "m1", "First", None).unwrap()["revision"]
        .as_u64()
        .unwrap();
    let second = room.offline_input(&sid, "m2", "Second", None).unwrap()["revision"]
        .as_u64()
        .unwrap();
    assert!(second > first);
    assert_eq!(
        publish(&room, &sid, "to-second", second)["status"],
        "queued"
    );
    let late = publish(&room, &sid, "to-first", first);
    assert_eq!(late["status"], "text_only");
    assert_eq!(late["status"], "text_only");
    assert!(late.get("reason").is_none());
}

#[test]
fn words_longer_than_a_message_are_refused_whichever_way_they_come() {
    let (_directory, room, sid, _events) = room_with_call();
    let longest = "a".repeat(12_000);
    let too_long = "a".repeat(12_001);
    let turn = room.begin_at_focus(&sid, "turn").unwrap();
    let refused = room
        .finish_turn(
            &sid,
            &turn.turn_id,
            Some(&too_long),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap_err();
    assert_eq!(refused.key, "room.text_too_long");
    // The turn did not end: its words, within bounds, still go through.
    room.finish_turn(
        &sid,
        &turn.turn_id,
        Some(&longest),
        &Value::Null,
        Continuation::Standalone,
    )
    .unwrap();
    assert_eq!(
        room.offline_input(&sid, "m1", &too_long, None)
            .unwrap_err()
            .key,
        "room.text_too_long"
    );
    let binding = room
        .inner
        .lock()
        .unwrap()
        .browsers
        .get(&sid)
        .unwrap()
        .target
        .as_ref()
        .unwrap()
        .binding_id
        .clone();
    let typed = room.send_text(
        &too_long,
        &sid,
        "x",
        &binding,
        "00000000-0000-4000-8000-000000000001",
    );
    assert_eq!(typed.unwrap_err().key, "room.text_too_long");
    assert_eq!(said(&room, "x"), [longest]);
}

#[test]
fn messages_waiting_for_their_agent_are_bounded_in_number_and_in_bytes() {
    let (_directory, room, sid, _events) = room_with_call();
    for n in 0..256 {
        room.offline_input(&sid, &format!("m{n}"), "Short words", None)
            .unwrap();
    }
    let refused = room.offline_input(&sid, "one-more", "Short words", None);
    assert_eq!(refused.unwrap_err().key, "room.input_backlog_full");

    let (_directory, room, sid, _events) = room_with_call();
    let long = "a".repeat(12_000);
    let mut fit = 0;
    while room
        .offline_input(&sid, &format!("m{fit}"), &long, None)
        .is_ok()
    {
        fit += 1;
    }
    assert!(fit > 0 && fit < 256);
    let turn = room.begin_at_focus(&sid, "turn").unwrap();
    let refused = room.finish_turn(
        &sid,
        &turn.turn_id,
        Some(&long),
        &Value::Null,
        Continuation::Standalone,
    );
    assert_eq!(refused.unwrap_err().key, "room.input_backlog_full");
    assert_eq!(said(&room, "x").len(), fit);
}

#[test]
fn a_turn_keeps_its_reserved_input_place_at_full_backlog() {
    for long in [false, true] {
        let (_directory, room, sid, _events) = room_with_call();
        let text = if long {
            "a".repeat(12_000)
        } else {
            "Short words".to_owned()
        };
        let turn = room.begin_at_focus(&sid, "kept").unwrap();
        let mut n = 0;
        while room
            .offline_input(&sid, &format!("m{n}"), &text, None)
            .is_ok()
        {
            n += 1;
        }
        room.select(&sid, "y").unwrap();
        // Other input is backpressured before consuming the slot reserved for this turn.
        let taken = room
            .finish_turn(
                &sid,
                "kept",
                Some(&text),
                &Value::Null,
                Continuation::Standalone,
            )
            .unwrap();
        assert_eq!(taken["accepted"], true, "long: {long}");
        assert_eq!(taken["revision"], turn.revision);
        assert_eq!(taken["thread_id"], "x");
    }
}

#[test]
fn a_turn_past_the_open_bound_is_refused_and_the_open_ones_keep_their_words() {
    let (_directory, room, sid, _events) = room_with_call();
    for n in 0..8 {
        room.begin_at_focus(&sid, &format!("t{n}")).unwrap();
    }
    assert_eq!(
        room.begin_at_focus(&sid, "ninth").unwrap_err().key,
        "room.turns_full"
    );
    // The focus moves while they wait: each turn's words still go where it was spoken.
    room.select(&sid, "y").unwrap();
    for n in 0..8 {
        room.finish_turn(
            &sid,
            &format!("t{n}"),
            Some(&format!("Words {n}")),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    }
    assert_eq!(said(&room, "x").len(), 8);
    assert!(said(&room, "y").is_empty());
    // With room again, a new turn starts.
    room.begin_at_focus(&sid, "ninth").unwrap();
}

#[test]
fn a_call_still_open_is_known_however_many_calls_came_and_went() {
    let (_directory, room, sid, _events) = room_with_call();
    for _ in 0..80 {
        let (events, _received) = mpsc::channel(4);
        let other = room.join("other".into(), "en".into(), events).unwrap();
        room.leave(&other);
    }
    let turn = room.begin_at_focus(&sid, "after").unwrap();
    let taken = room
        .finish_turn(
            &sid,
            "after",
            Some("Still here"),
            &Value::Null,
            Continuation::Standalone,
        )
        .unwrap();
    assert_eq!(taken["accepted"], true);
    // A reply to it is the call's own, not one for a session that changed.
    assert_eq!(
        publish(&room, &sid, "to-after", turn.revision)["status"],
        "queued"
    );
}
