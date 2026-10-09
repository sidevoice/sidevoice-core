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
    let a = room.begin_turn(&sid).unwrap();
    let b = room.begin_turn(&sid).unwrap();
    room.finish_turn(&sid, a.revision, Some("First words"), &Value::Null)
        .unwrap();
    room.finish_turn(&sid, b.revision, Some("Second words"), &Value::Null)
        .unwrap();
    assert_eq!(said(&room, "x"), ["First words", "Second words"]);
    // Each turn ends once.
    assert_eq!(
        room.finish_turn(&sid, a.revision, Some("First words"), &Value::Null)
            .unwrap_err()
            .key,
        "room.input_ended"
    );
}

#[test]
fn a_turn_finished_after_the_focus_moved_goes_once_to_the_conversation_it_was_spoken_to() {
    let (_directory, room, sid, _events) = room_with_call();
    let a = room.begin_turn(&sid).unwrap();
    room.select(&sid, "y").unwrap();
    room.finish_turn(&sid, a.revision, Some("Words for x"), &Value::Null)
        .unwrap();
    assert_eq!(said(&room, "x"), ["Words for x"]);
    assert!(said(&room, "y").is_empty());
    assert_eq!(delivered_to(&room), ["x"]);
}

#[test]
fn a_cancelled_turn_drops_its_words_and_leaves_the_others() {
    let (_directory, room, sid, _events) = room_with_call();
    let a = room.begin_turn(&sid).unwrap();
    let b = room.begin_turn(&sid).unwrap();
    room.cancel_input(&sid, a.revision).unwrap();
    assert_eq!(
        room.finish_turn(&sid, a.revision, Some("Dropped"), &Value::Null)
            .unwrap_err()
            .key,
        "room.input_ended"
    );
    room.finish_turn(&sid, b.revision, Some("Kept"), &Value::Null)
        .unwrap();
    assert_eq!(said(&room, "x"), ["Kept"]);
}

#[test]
fn an_offline_message_is_newer_than_the_turn_before_it() {
    let (_directory, room, sid, _events) = room_with_call();
    let live = room.begin_turn(&sid).unwrap();
    room.finish_turn(&sid, live.revision, Some("Live words"), &Value::Null)
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
