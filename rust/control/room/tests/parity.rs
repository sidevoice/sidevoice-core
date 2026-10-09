//! The room's behaviour across calls and conversations: catch-up of missed replies,
//! the playback bound, the pause after speaking, focus and working state, and input limits.
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::pull_room;
use crate::control::room::journal::parse_input_ttl;
use crate::control::room::playback::playback_bound;
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
    room.set_audio_grace(&sid, 0.0);
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

fn active(room: &Room, sid: &str) -> Option<String> {
    room.inner
        .lock()
        .unwrap()
        .browsers
        .get(sid)
        .unwrap()
        .active
        .clone()
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
    room.inner.lock().unwrap().dispatch_client(&back);
    assert!(drain(&mut events).iter().all(|event| !matches!(
        event["type"].as_str(),
        Some("voice-speech" | "voice-replay")
    )));
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
fn an_unconfirmed_playback_times_out_and_the_queue_moves_on() {
    let (_directory, room, _) = parity_room("t");
    let (sid, _events) = browser(&room, "t");
    reply(&room, &sid, "t", "lost", "Never confirmed");
    reply(&room, &sid, "t", "next", "Next reply");
    assert_eq!(playback_bound("abcdef"), Duration::from_secs(61));
    assert_eq!(active(&room, &sid).as_deref(), Some("lost"));
    let deadline = room
        .inner
        .lock()
        .unwrap()
        .browsers
        .get(&sid)
        .unwrap()
        .playback_watch
        .as_ref()
        .unwrap()
        .1;
    room.inner
        .lock()
        .unwrap()
        .expire_playback(&sid, deadline - Duration::from_millis(1));
    assert_eq!(active(&room, &sid).as_deref(), Some("lost"));
    {
        let mut inner = room.inner.lock().unwrap();
        inner.expire_playback(&sid, deadline);
        inner.dispatch_client(&sid);
    }
    assert_eq!(active(&room, &sid).as_deref(), Some("next"));
    assert_eq!(
        row(&room, &format!("{sid}:voice:lost")),
        ("failed".into(), Some("unconfirmed".into()))
    );
}

#[test]
fn a_parked_call_keeps_its_reply_and_is_handed_nothing_new_until_it_is_back() {
    let (_directory, room, _) = parity_room("t");
    let (sid, _events) = browser(&room, "t");
    reply(&room, &sid, "t", "playing", "Handed before the drop");
    room.park(&sid, true);
    reply(&room, &sid, "t", "later", "Published while parked");
    let deadline = room
        .inner
        .lock()
        .unwrap()
        .browsers
        .get(&sid)
        .unwrap()
        .playback_watch
        .as_ref()
        .unwrap()
        .1;
    {
        let mut inner = room.inner.lock().unwrap();
        // Long past its bound: nobody can confirm it while the page is away, so it is not failed.
        inner.expire_playback(&sid, deadline + Duration::from_secs(600));
        inner.dispatch_client(&sid);
    }
    assert_eq!(active(&room, &sid).as_deref(), Some("playing"));
    assert_ne!(row(&room, &format!("{sid}:voice:playing")).0, "failed");
    std::thread::sleep(Duration::from_millis(20));
    room.park(&sid, false);
    let moved = room
        .inner
        .lock()
        .unwrap()
        .browsers
        .get(&sid)
        .unwrap()
        .playback_watch
        .as_ref()
        .unwrap()
        .1;
    assert!(
        moved >= deadline + Duration::from_millis(20),
        "the bound restarts where it stopped"
    );
    let current = revision(&room, &sid);
    room.receipt(&sid, "playing", current, "playing").unwrap();
    room.receipt(&sid, "playing", current, "playback_finished")
        .unwrap();
    room.inner.lock().unwrap().dispatch_client(&sid);
    assert_eq!(active(&room, &sid).as_deref(), Some("later"));
}

#[test]
fn a_reply_waits_out_the_pause_after_the_person_stops_speaking() {
    let (_directory, room, _) = parity_room("t");
    let (sid, _events) = browser(&room, "t");
    room.set_audio_grace(&sid, 5.0);
    let rev = revision(&room, &sid);
    assert_eq!(
        reply(&room, &sid, "t", "barged", "Playing when the person speaks")["status"],
        "queued"
    );
    room.receipt(&sid, "barged", rev, "playing").unwrap();
    let turn = room.begin_turn(&sid).unwrap();
    assert_eq!(
        row(&room, &format!("{sid}:voice:barged")),
        ("interrupted".into(), Some("user_interrupted".into()))
    );
    reply(&room, &sid, "t", "after", "Reply after the turn");
    room.finish_turn(&sid, turn.revision);
    let row_id = format!("{sid}:voice:after");
    assert_eq!(
        row(&room, &row_id),
        ("waiting_for_pause".into(), Some("quiet_grace".into()))
    );
    assert_eq!(active(&room, &sid), None);
    {
        let mut inner = room.inner.lock().unwrap();
        inner.browsers.get_mut(&sid).unwrap().quiet_until = Some(Instant::now());
        inner.dispatch_client(&sid);
    }
    assert_eq!(active(&room, &sid).as_deref(), Some("after"));
    assert_eq!(row(&room, &row_id).0, "queued");
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
