//! Turns waiting for their words: a newer turn, a change of focus or a turn spoken offline never loses or misplaces
//! the words of a turn already started, and an offline message is ordered after every turn before it.
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::pull_room;
use crate::control::room::Room;

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

fn publish(room: &Room, sid: &str, uid: &str, revision: u64) -> Value {
    room.publish(
        &json!({"session_id":sid,"thread_id":"x","revision":revision,"utterance_id":uid,"text":"A reply."}),
        false,
    )
}

#[test]
fn a_newer_turn_does_not_lose_the_words_of_one_still_transcribed() {
    let (_directory, room, sid, _events) = room_with_call();
    let a = room.begin_turn(&sid, "a").unwrap();
    let b = room.begin_turn(&sid, "b").unwrap();
    room.finish_turn(&sid, &a.turn_id, Some("First words"), &Value::Null)
        .unwrap();
    room.finish_turn(&sid, &b.turn_id, Some("Second words"), &Value::Null)
        .unwrap();
    assert_eq!(said(&room, "x"), ["First words", "Second words"]);
    // Each turn ends once.
    assert_eq!(
        room.finish_turn(&sid, &a.turn_id, Some("First words"), &Value::Null)
            .unwrap_err()
            .key,
        "room.input_ended"
    );
}

#[test]
fn a_turn_finished_after_the_focus_moved_goes_once_to_the_conversation_it_was_spoken_to() {
    let (_directory, room, sid, _events) = room_with_call();
    let a = room.begin_turn(&sid, "a").unwrap();
    room.select(&sid, "y").unwrap();
    room.finish_turn(&sid, &a.turn_id, Some("Words for x"), &Value::Null)
        .unwrap();
    assert_eq!(said(&room, "x"), ["Words for x"]);
    assert!(said(&room, "y").is_empty());
    assert_eq!(delivered_to(&room), ["x"]);
}

#[test]
fn a_cancelled_turn_drops_its_words_and_leaves_the_others() {
    let (_directory, room, sid, _events) = room_with_call();
    let a = room.begin_turn(&sid, "a").unwrap();
    let b = room.begin_turn(&sid, "b").unwrap();
    room.cancel_input(&sid, &a.turn_id).unwrap();
    assert_eq!(
        room.finish_turn(&sid, &a.turn_id, Some("Dropped"), &Value::Null)
            .unwrap_err()
            .key,
        "room.input_ended"
    );
    room.finish_turn(&sid, &b.turn_id, Some("Kept"), &Value::Null)
        .unwrap();
    assert_eq!(said(&room, "x"), ["Kept"]);
}

#[test]
fn an_offline_message_is_newer_than_the_turn_before_it() {
    let (_directory, room, sid, _events) = room_with_call();
    let live = room.begin_turn(&sid, "live").unwrap();
    room.finish_turn(&sid, &live.turn_id, Some("Live words"), &Value::Null)
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
    assert_eq!(late["reason"], "newer_turn");
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
    assert_eq!(late["reason"], "newer_turn");
}

#[test]
fn words_longer_than_a_message_are_refused_whichever_way_they_come() {
    let (_directory, room, sid, _events) = room_with_call();
    let longest = "a".repeat(12_000);
    let too_long = "a".repeat(12_001);
    let turn = room.begin_turn(&sid, "turn").unwrap();
    let refused = room
        .finish_turn(&sid, &turn.turn_id, Some(&too_long), &Value::Null)
        .unwrap_err();
    assert_eq!(refused.key, "room.text_too_long");
    // The turn did not end: its words, within bounds, still go through.
    room.finish_turn(&sid, &turn.turn_id, Some(&longest), &Value::Null)
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
    let fit = (1024 * 1024) / long.len();
    for n in 0..fit {
        room.offline_input(&sid, &format!("m{n}"), &long, None)
            .unwrap();
    }
    let turn = room.begin_turn(&sid, "turn").unwrap();
    let refused = room.finish_turn(&sid, &turn.turn_id, Some(&long), &Value::Null);
    assert_eq!(refused.unwrap_err().key, "room.input_backlog_full");
    assert_eq!(said(&room, "x").len(), fit);
}
