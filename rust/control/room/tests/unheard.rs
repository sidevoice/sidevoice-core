//! What the person did not hear: which replies count, what the agent is told with the next
//! message, and the note sent when the person comes back and says nothing.
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::{pull_room, report, say};
use crate::control::room::unheard::{unheard, TOLD_CHARS, TOLD_REPLIES};
use crate::control::room::Room;

/// A room with one push binding on `thread`: the room, its binding and the connector's generation.
fn room_on(thread: &str) -> (tempfile::TempDir, Room, String, String) {
    let (directory, room, generations) = pull_room(&["connector"]);
    let bid = room
        .register("connector", &json!({"thread":thread,"harness":"claude"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    (directory, room, bid, generations[0].clone())
}

fn call(room: &Room, thread: &str) -> (String, mpsc::Receiver<Value>) {
    let (events, received) = mpsc::channel(256);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, thread).unwrap();
    (sid, received)
}

fn revision(room: &Room, sid: &str) -> u64 {
    room.inner
        .lock()
        .unwrap()
        .browsers
        .get(sid)
        .unwrap()
        .revision
}

fn reply(room: &Room, sid: &str, thread: &str, uid: &str, text: &str, revision: u64) -> Value {
    room.publish(
        &json!({"session_id":sid,"thread_id":thread,"revision":revision,"utterance_id":uid,"text":text}),
        false,
    )
}

fn status(room: &Room, sid: &str, uid: &str) -> (String, Option<String>) {
    let inner = room.inner.lock().unwrap();
    let row = inner.journal.find(&format!("{sid}:voice:{uid}")).unwrap();
    (row.status.clone(), row.reason.clone())
}

/// What the message the person said as `text` tells its agent they did not hear.
fn told(room: &Room, text: &str) -> Option<Value> {
    let inner = room.inner.lock().unwrap();
    let row = inner.journal.find_input(text).expect("the message");
    row.payload.as_ref().unwrap().get("unheard").cloned()
}

fn only<T>(items: Vec<T>) -> T {
    assert_eq!(items.len(), 1);
    items.into_iter().next().unwrap()
}

/// The data of every `input.deliver` due now on `channel`.
fn deliveries(room: &Room, channel: &str) -> Vec<Value> {
    room.pending_delivery()
        .into_iter()
        .map(|(_, _, _, data)| data)
        .filter(|data| data["channel"] == channel)
        .collect()
}

#[test]
fn what_counts_as_unheard() {
    assert_eq!(unheard("text_only", None), Some(false));
    assert_eq!(unheard("interrupted", Some("user_interrupted")), Some(true));
    for reason in [
        "newer_turn",
        "unheard",
        "focus_changed",
        "call_ended",
        "session_changed",
    ] {
        assert_eq!(
            unheard("interrupted", Some(reason)),
            Some(false),
            "{reason}"
        );
    }
    assert_eq!(unheard("failed", Some("playback_failed")), Some(false));
    // Skipped on purpose, or perhaps played: not reported.
    assert_eq!(unheard("interrupted", Some("user_skipped")), None);
    assert_eq!(unheard("failed", Some("unconfirmed")), None);
    assert_eq!(unheard("playback_finished", None), None);
    assert_eq!(unheard("queued", None), None);
}

#[test]
fn a_reply_spoken_over_is_cut_and_the_next_message_tells_the_agent() {
    let (_directory, room, _, _) = room_on("t");
    let (sid, _received) = call(&room, "t");
    reply(
        &room,
        &sid,
        "t",
        "r1",
        "The build is green.",
        revision(&room, &sid),
    );
    report(&room, &sid, "r1", "playing");
    room.playback(&sid, "r1", "interrupted", None, Some(9), &Value::Null)
        .unwrap();
    say(&room, &sid, "And the tests?");
    assert_eq!(
        status(&room, &sid, "r1"),
        ("interrupted".into(), Some("user_interrupted".into()))
    );
    let message = only(deliveries(&room, "voice"));
    assert_eq!(message["text"], "And the tests?");
    assert_eq!(
        message["unheard"],
        json!({"count":1,"replies":[{"text":"The build is green.","truncated":false,"cut":true,"heard_chars":9}]})
    );
    // Told once: the next message carries nothing.
    say(&room, &sid, "Thanks.");
    assert_eq!(told(&room, "Thanks."), None);
}

#[test]
fn replies_the_call_drops_for_a_newer_turn_are_never_played() {
    let (_directory, room, _, _) = room_on("t");
    let (sid, _received) = call(&room, "t");
    let asked = revision(&room, &sid);
    reply(&room, &sid, "t", "r1", "First.", asked);
    reply(&room, &sid, "t", "r2", "Second.", asked);
    for uid in ["r1", "r2"] {
        report(&room, &sid, uid, "unplayed");
        assert_eq!(
            status(&room, &sid, uid),
            ("interrupted".into(), Some("newer_turn".into()))
        );
    }
    say(&room, &sid, "Wait, something else.");
    let message = only(deliveries(&room, "voice"));
    assert_eq!(message["unheard"]["count"], 2);
    let texts: Vec<_> = message["unheard"]["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].clone())
        .collect();
    assert_eq!(texts, ["First.", "Second."]);
}

#[test]
fn a_reply_to_a_turn_the_person_already_followed_is_not_spoken() {
    let (_directory, room, _, _) = room_on("t");
    let (sid, _received) = call(&room, "t");
    let asked = revision(&room, &sid);
    say(&room, &sid, "Something new.");
    let answer = reply(
        &room,
        &sid,
        "t",
        "late",
        "An answer to the old question.",
        asked,
    );
    assert_eq!(answer["status"], "text_only");
    assert_eq!(
        status(&room, &sid, "late"),
        ("text_only".into(), Some("newer_turn".into()))
    );
    say(&room, &sid, "Go on.");
    let unheard = told(&room, "Go on.").unwrap();
    assert_eq!(
        unheard["replies"][0]["text"],
        "An answer to the old question."
    );
}

#[test]
fn what_was_published_while_away_becomes_a_bounded_note_on_return() {
    let (_directory, room, bid, generation) = room_on("t");
    let (sid, _received) = call(&room, "elsewhere");
    let long = "x".repeat(TOLD_CHARS + 20);
    for n in 0..5 {
        reply(
            &room,
            &sid,
            "t",
            &format!("p{n}"),
            &format!("{n}{long}"),
            revision(&room, &sid),
        );
    }
    room.select(&sid, "t").unwrap();
    // Not before the person has had a moment to speak first.
    assert!(deliveries(&room, "note").is_empty());
    room.inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    let (work_bid, note_id, _, note) = only(room.pending_delivery());
    assert_eq!(work_bid, bid);
    assert_eq!(note["channel"], "note");
    assert_eq!(note["text"], "");
    assert_eq!(note["session_id"], sid.as_str());
    assert_eq!(note["revision"], revision(&room, &sid));
    assert_eq!(note["message_id"], note_id.as_str());
    assert_eq!(note["unheard"]["count"], 5);
    let replies = note["unheard"]["replies"].as_array().unwrap();
    assert_eq!(replies.len(), TOLD_REPLIES);
    assert!(replies[0]["text"].as_str().unwrap().starts_with('2'));
    assert!(replies.iter().all(
        |r| r["truncated"] == true && r["text"].as_str().unwrap().chars().count() == TOLD_CHARS
    ));
    room.settle_delivery(
        &bid,
        &note_id,
        &generation,
        Ok(json!({"status":"accepted"})),
    );
    assert!(room.inner.lock().unwrap().unheard.note_mut("t").is_none());
    say(&room, &sid, "Where are we?");
    assert_eq!(told(&room, "Where are we?"), None);
}

#[test]
fn a_note_not_taken_is_sent_again_with_the_same_list() {
    let (_directory, room, bid, generation) = room_on("t");
    let (sid, _received) = call(&room, "elsewhere");
    reply(&room, &sid, "t", "p", "Done.", revision(&room, &sid));
    room.select(&sid, "t").unwrap();
    room.inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    let (_, note_id, _, first) = only(room.pending_delivery());
    room.settle_delivery(&bid, &note_id, &generation, Ok(json!({"status":"failed"})));
    room.inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    let (_, again_id, _, again) = only(room.pending_delivery());
    assert_eq!(again_id, note_id);
    assert_eq!(again["unheard"], first["unheard"]);
}

#[test]
fn a_message_before_the_note_is_due_takes_the_list_and_the_note_is_dropped() {
    let (_directory, room, _, _) = room_on("t");
    let (sid, _received) = call(&room, "elsewhere");
    reply(&room, &sid, "t", "p", "Done.", revision(&room, &sid));
    room.select(&sid, "t").unwrap();
    say(&room, &sid, "Hi.");
    let message = only(deliveries(&room, "voice"));
    assert_eq!(message["unheard"]["count"], 1);
    room.inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    assert!(deliveries(&room, "note").is_empty());
    assert!(room.inner.lock().unwrap().unheard.note_mut("t").is_none());
}

#[test]
fn a_reply_played_to_the_end_is_not_reported() {
    let (_directory, room, _, _) = room_on("t");
    let (sid, _received) = call(&room, "t");
    let asked = revision(&room, &sid);
    reply(&room, &sid, "t", "r1", "All good.", asked);
    report(&room, &sid, "r1", "playing");
    report(&room, &sid, "r1", "heard");
    assert!(!room.inner.lock().unwrap().unheard.has("t"));
}

#[test]
fn how_far_a_cut_reply_was_heard_reaches_the_agent_when_known() {
    let (_directory, room, _, _) = room_on("t");
    let (sid, _received) = call(&room, "t");
    let asked = revision(&room, &sid);
    reply(
        &room,
        &sid,
        "t",
        "r1",
        "The build is green and deployed.",
        asked,
    );
    report(&room, &sid, "r1", "playing");
    room.playback(&sid, "r1", "interrupted", None, Some(12), &Value::Null)
        .unwrap();
    say(&room, &sid, "Stop.");
    let unheard = told(&room, "Stop.").unwrap();
    assert_eq!(
        unheard["replies"][0],
        json!({"text":"The build is green and deployed.","truncated":false,"cut":true,"heard_chars":12})
    );
}
