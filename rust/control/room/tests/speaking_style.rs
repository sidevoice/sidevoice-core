//! The person's speaking style reaches the connector with every message, by push and by pull, and only when valid.
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::{pull, pull_room, BeginAtFocus};
use crate::control::room::{Continuation, Room, SpeakingStyle};

const OFFLINE: &str = "offline-turn";

fn call(room: &Room) -> (String, String) {
    let (events, _received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "t").unwrap();
    let bid = room.snapshot(Some(&sid))["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    (sid, bid)
}

fn spoken(room: &Room, sid: &str, turn_id: &str, text: &str, style: &Value) {
    let turn = room.begin_at_focus(sid, turn_id).unwrap();
    room.finish_turn(
        sid,
        &turn.turn_id,
        Some(text),
        &Value::Null,
        Continuation::Standalone,
        SpeakingStyle::parse(style),
    )
    .unwrap();
}

/// The `speaking_style` of each message in the order the room offers them, `None` when the field is absent.
fn styles(messages: &[Value]) -> Vec<Option<String>> {
    messages
        .iter()
        .map(|message| {
            message
                .get("speaking_style")
                .map(|s| s.as_str().unwrap().to_owned())
        })
        .collect()
}

fn typed_and_spoken(room: &Room, sid: &str, bid: &str) {
    room.send_text("typed", sid, "t", bid, "m-1", Some(SpeakingStyle::Brief))
        .unwrap();
    spoken(room, sid, "turn-1", "spoken", &json!("detailed"));
    room.send_text(
        "bogus",
        sid,
        "t",
        bid,
        "m-2",
        SpeakingStyle::parse(&json!("loud")),
    )
    .unwrap();
    spoken(room, sid, "turn-2", "number", &json!(3));
    room.send_text("absent", sid, "t", bid, "m-3", None)
        .unwrap();
    room.offline_input_captured(
        sid,
        OFFLINE,
        "late",
        None,
        "t",
        bid,
        Some(SpeakingStyle::Normal),
    )
    .unwrap();
}

const EXPECTED: [Option<&str>; 6] = [
    Some("brief"),
    Some("detailed"),
    None,
    None,
    None,
    Some("normal"),
];

#[test]
fn push_carries_the_style_of_typed_voice_and_late_messages() {
    let (_directory, room, _) = pull_room(&["connector"]);
    room.register(
        "connector",
        &json!({"thread":"t","harness":"claude","capabilities":{"deliver":"supported"}}),
    )
    .unwrap();
    let (sid, bid) = call(&room);
    let mut seen = Vec::new();
    // The room offers one message at a time: settle each to reach the next.
    typed_and_spoken(&room, &sid, &bid);
    for _ in 0..EXPECTED.len() {
        let work = room.pending_delivery();
        assert_eq!(work.len(), 1);
        let (bid, rid, peer, data) = work.into_iter().next().unwrap();
        seen.push(data);
        room.settle_delivery(
            &bid,
            &rid,
            &peer.generation,
            Ok(json!({"status":"accepted"})),
        );
    }
    let got = styles(&seen);
    let want: Vec<Option<String>> = EXPECTED.iter().map(|s| s.map(str::to_owned)).collect();
    assert_eq!(got, want);
}

#[test]
fn pull_carries_the_style_of_typed_voice_and_late_messages() {
    let (_directory, room, _) = pull_room(&["connector"]);
    let joined = room
        .register(
            "connector",
            &json!({"thread":"t","harness":"claude","capabilities":{"deliver":"unsupported"}}),
        )
        .unwrap();
    let bid = joined["binding_id"].as_str().unwrap().to_owned();
    let (sid, focused) = call(&room);
    typed_and_spoken(&room, &sid, &focused);
    let got = pull(
        &room,
        "connector",
        json!({"binding_id":bid,"operation":"get"}),
    )
    .unwrap();
    let messages = got["messages"].as_array().unwrap();
    assert_eq!(messages.len(), EXPECTED.len());
    let want: Vec<Option<String>> = EXPECTED.iter().map(|s| s.map(str::to_owned)).collect();
    assert_eq!(styles(messages), want);
}
