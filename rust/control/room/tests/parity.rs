//! The room's behaviour across calls and conversations: what a returning or parked call is sent, focus and
//! working state, and input limits.

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::pull_room;
use crate::control::room::journal::parse_input_ttl;
use crate::control::room::Room;

/// A room with one linked connector, one binding on `thread`, and the binding's ID.
fn parity_room(thread: &str) -> (tempfile::TempDir, Room, String) {
    let (directory, room, _) = pull_room(&["connector"]);
    let bid = room
        .register("connector", &json!({"thread":thread,"harness":"codex"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    (directory, room, bid)
}

fn browser(room: &Room, thread: &str) -> (String, mpsc::Receiver<Value>) {
    let (events, received) = mpsc::channel(256);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    if !thread.is_empty() {
        room.select(&sid, thread).unwrap();
    }
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

fn reply(room: &Room, sid: &str, thread: &str, uid: &str, text: &str) -> Value {
    room.publish(
        &json!({"session_id":sid,"thread_id":thread,"revision":revision(room, sid),
            "utterance_id":uid,"text":text}),
        false,
    )
}

fn row(room: &Room, row_id: &str) -> (String, Option<String>) {
    let inner = room.inner.lock().unwrap();
    let row = inner.journal.find(row_id).unwrap();
    (row.status.clone(), row.reason.clone())
}

fn drain(received: &mut mpsc::Receiver<Value>) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(event) = received.try_recv() {
        events.push(event);
    }
    events
}

#[test]
fn a_returning_browser_is_not_played_what_it_missed() {
    let (_directory, room, _) = parity_room("t");
    let (first, _first_events) = browser(&room, "t");
    reply(&room, &first, "t", "missed", "Missed reply");
    room.leave(&first);
    let (back, mut events) = browser(&room, "");
    assert!(room.restore_focus(&back, "t"));
    assert!(drain(&mut events)
        .iter()
        .all(|event| event["type"] != "voice-reply"));
}

#[test]
fn selecting_needs_no_binding_and_shows_working_which_detach_clears() {
    let (_directory, room, bid) = parity_room("t");
    let (sid, mut events) = browser(&room, "");
    assert_eq!(
        room.select(&sid, "no-binding-yet").unwrap()["status"],
        "activated"
    );
    room.working("connector", &json!({"binding_id":bid,"working":true}));
    drain(&mut events);
    room.select(&sid, "t").unwrap();
    assert!(drain(&mut events)
        .iter()
        .any(|e| e["type"] == "voice-conversation"
            && e["data"]["thread_id"] == "t"
            && e["data"]["working"] == true));
    let (other, mut other_events) = browser(&room, "");
    assert!(room.restore_focus(&other, "t"));
    assert!(drain(&mut other_events)
        .iter()
        .any(|e| e["type"] == "voice-conversation" && e["data"]["working"] == true));
    assert!(!room.restore_focus(&other, "unknown-thread"));
    assert!(room.has_connector());
    let generation = room
        .inner
        .lock()
        .unwrap()
        .peers
        .get("connector")
        .unwrap()
        .generation
        .clone();
    room.detach("connector", &generation);
    assert!(!room.has_connector());
    assert_eq!(room.inner.lock().unwrap().bindings.working("t"), None);
}

#[test]
fn a_reply_goes_out_as_text_at_once_and_a_parked_call_is_sent_nothing_until_it_is_back() {
    let (_directory, room, _) = parity_room("t");
    let (sid, mut events) = browser(&room, "t");
    let turn = room.begin_turn(&sid, "turn").unwrap();
    // The person is speaking: the reply still goes out, for the call's voice module to hold or drop.
    reply(&room, &sid, "t", "sent", "Sent while the person spoke");
    let sent = drain(&mut events);
    let sent = sent
        .iter()
        .find(|event| event["type"] == "voice-reply")
        .expect("the reply is sent");
    assert_eq!(
        sent["data"],
        json!({"session_id":sid,"utterance_id":"sent","revision":turn.revision,"reply_revision":turn.revision,
            "thread_id":"t","text":"Sent while the person spoke","language":null,
            "history_id":format!("{sid}:voice:sent")})
    );
    assert_eq!(row(&room, &format!("{sid}:voice:sent")).0, "queued");
    room.park(&sid, true);
    reply(&room, &sid, "t", "later", "Published while parked");
    assert!(drain(&mut events).is_empty());
    // Back: what was published while it was away, and a reply whose frame never reached it, are unheard.
    room.resume(&sid, &[]);
    assert!(drain(&mut events).is_empty());
    assert_eq!(
        row(&room, &format!("{sid}:voice:later")),
        ("interrupted".into(), Some("unheard".into()))
    );
    assert_eq!(row(&room, &format!("{sid}:voice:sent")).0, "queued");
    room.park(&sid, true);
    room.resume(&sid, &["sent".to_owned()]);
    assert_eq!(
        row(&room, &format!("{sid}:voice:sent")),
        ("interrupted".into(), Some("unheard".into()))
    );
}

#[test]
fn speech_is_limited_in_characters_and_input_ttl_is_configurable() {
    let (_directory, room, _) = parity_room("t");
    let (sid, _events) = browser(&room, "t");
    assert_ne!(
        reply(&room, &sid, "t", "long", &"é".repeat(6000))["status"],
        "rejected"
    );
    assert_eq!(
        reply(&room, &sid, "t", "longer", &"é".repeat(6001))["status"],
        "rejected"
    );
    assert_eq!(parse_input_ttl(None), 600);
    assert_eq!(parse_input_ttl(Some("")), 600);
    assert_eq!(parse_input_ttl(Some("45")), 45);
    assert_eq!(parse_input_ttl(Some("soon")), 600);
}

#[test]
fn holding_reachability_keeps_the_connectors_reason_and_remedy() {
    let (_directory, room, _) = parity_room("t");
    room.register(
        "connector",
        &json!({"thread":"held","harness":"codex",
            "inbound":{"ok":false,"reason":"Harness holds input","remedy":{"command":"allow"}}}),
    )
    .unwrap();
    room.register(
        "connector",
        &json!({"thread":"plain","harness":"codex","inbound":{"ok":false}}),
    )
    .unwrap();
    let participants = room.participants(None);
    let reach = |thread: &str| {
        participants
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["thread_id"] == thread)
            .unwrap()["reach"]
            .clone()
    };
    assert_eq!(
        reach("held"),
        json!({"state":"holding","detail":"Harness holds input","remedy":{"command":"allow"}})
    );
    assert_eq!(reach("plain")["state"], "holding");
    assert!(reach("plain")["detail"]
        .as_str()
        .is_some_and(|detail| !detail.is_empty()));
    assert_eq!(reach("plain")["remedy"], Value::Null);
}
