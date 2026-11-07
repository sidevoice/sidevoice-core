use serde_json::json;
use tokio::sync::mpsc;

use super::support::{pull_room, say};

#[test]
fn push_serializes_one_conversation_even_with_an_idle_older_binding() {
    let (_directory, room, _) = pull_room(&["connector"]);
    let older = room
        .register(
            "connector",
            &json!({"thread":"busy","harness":"claude","capabilities":{"deliver":"supported"}}),
        )
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    room.inner
        .lock()
        .unwrap()
        .bindings
        .get_mut(&older)
        .unwrap()
        .created -= 10;
    let (events, _received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    // A second, newer push binding for the same thread from the same connector.
    let newer = {
        let mut inner = room.inner.lock().unwrap();
        let binding = inner.bindings.get_or_create("newer", "connector", "busy");
        binding.harness = "claude".into();
        binding.capabilities = json!({"deliver":"supported"});
        binding.created += 1;
        "newer".to_owned()
    };
    room.select(&sid, "busy").unwrap();
    say(&room, &sid, "one");
    say(&room, &sid, "two");
    let delivery = room.pending_delivery();
    let targets: Vec<&str> = delivery.iter().map(|work| work.0.as_str()).collect();
    assert_eq!(targets, [newer.as_str()]);
    let (bid, rid, peer, _) = &delivery[0];
    room.settle_delivery(bid, rid, &peer.generation, Ok(json!({"status":"accepted"})));
    let following = room.pending_delivery();
    assert_eq!(following.len(), 1);
    assert_eq!(following[0].0, newer);
    assert_ne!(following[0].0, older);
}

/// A room with one live push binding for thread `t`, and a call on it. Returns the room's directory too.
fn garage(directory: &tempfile::TempDir, room: &crate::control::room::Room) -> String {
    room.register(
        "connector",
        &json!({"thread":"t","harness":"claude","capabilities":{"deliver":"supported"}}),
    )
    .unwrap();
    let (events, _received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "t").unwrap();
    let _ = directory;
    sid
}

/// What the room offers the connector now: the texts, in order.
fn offered(room: &crate::control::room::Room) -> Vec<(String, String, String, serde_json::Value)> {
    room.pending_delivery()
        .into_iter()
        .map(|(bid, rid, peer, data)| {
            (
                bid,
                rid,
                peer.generation.clone(),
                json!({"text": data["text"], "message_id": data["message_id"]}),
            )
        })
        .collect()
}

/// Makes every waiting message due again, as when the back-off has run.
fn due(room: &crate::control::room::Room) {
    let mut inner = room.inner.lock().unwrap();
    let ids: Vec<String> = inner
        .journal
        .waiting_user_input()
        .map(|row| row.id.clone())
        .collect();
    for id in ids {
        inner.journal.find_mut(&id).unwrap().next_attempt = 0;
    }
}

#[test]
fn the_garage_several_messages_wait_while_the_harness_is_unreachable_and_arrive_once_and_in_order()
{
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    for words in ["one", "two", "three"] {
        say(&room, &sid, words);
    }
    let mut arrived: Vec<String> = Vec::new();
    // The link is down: every attempt fails, for a long time, and nothing later is offered ahead of the first.
    for _ in 0..8 {
        let work = room.pending_delivery();
        assert_eq!(work.len(), 1, "one conversation, one message in flight");
        assert_eq!(
            work[0].3["text"], "one",
            "the first waits, the later ones with it"
        );
        let (bid, rid, peer, _) = &work[0];
        room.settle_delivery(bid, rid, &peer.generation, Ok(json!({"status":"failed"})));
        assert!(
            room.pending_delivery().is_empty(),
            "a failed one waits its back-off, and holds the thread"
        );
        // However long it has waited, it is not given up.
        {
            let mut inner = room.inner.lock().unwrap();
            for row in inner
                .journal
                .waiting_user_input()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>()
            {
                inner.journal.find_mut(&row).unwrap().queued_at = 0;
            }
        }
        due(&room);
    }
    // The link is back: each goes once, the next only after the one before is taken.
    for expected in ["one", "two", "three"] {
        let work = offered(&room);
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].3["text"], expected);
        arrived.push(work[0].3["text"].as_str().unwrap().to_owned());
        room.settle_delivery(
            &work[0].0,
            &work[0].1,
            &work[0].2,
            Ok(json!({"status":"accepted"})),
        );
    }
    assert_eq!(arrived, ["one", "two", "three"]);
    assert!(room.pending_delivery().is_empty(), "each was taken once");
}

#[test]
fn the_retry_schedule_is_fast_at_first_and_then_every_fifteen_seconds() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    let mut waits = Vec::new();
    for _ in 0..7 {
        let work = room.pending_delivery();
        let (bid, rid, peer, _) = &work[0];
        room.settle_delivery(bid, rid, &peer.generation, Ok(json!({"status":"failed"})));
        let inner = room.inner.lock().unwrap();
        let row = inner.journal.waiting_user_input().next().unwrap();
        waits.push(row.next_attempt - crate::control::room::util::seconds());
        drop(inner);
        due(&room);
    }
    assert_eq!(waits, [1, 2, 5, 10, 15, 15, 15]);
}

#[test]
fn a_restart_in_the_middle_keeps_what_waited_in_order() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    for words in ["one", "two", "three"] {
        say(&room, &sid, words);
    }
    // "one" was taken by the agent; "two" and "three" wait when the core goes down.
    let work = offered(&room);
    room.settle_delivery(
        &work[0].0,
        &work[0].1,
        &work[0].2,
        Ok(json!({"status":"accepted"})),
    );
    // No pump tick between: the record was written as each change was made.
    drop(room);
    let reloaded = crate::control::room::Room::load(
        crate::storage::PrivateDir::open(directory.path().join("private")).unwrap(),
    )
    .unwrap();
    let (requests, _receiver) = mpsc::channel(4);
    let (stop, _stopped) = tokio::sync::watch::channel(false);
    reloaded.attach(
        "connector",
        crate::control::room::ConnectorPeer {
            generation: crate::control::room::util::id(),
            sender: requests,
            stop,
        },
    );
    reloaded
        .register(
            "connector",
            &json!({"thread":"t","harness":"claude","capabilities":{"deliver":"supported"}}),
        )
        .unwrap();
    assert_eq!(
        reloaded.inner.lock().unwrap().conversations.epoch("t"),
        3,
        "the conversation's epoch is at least that of what it holds, so a reply to it is current"
    );
    let mut arrived = Vec::new();
    for _ in 0..2 {
        let work = offered(&reloaded);
        assert_eq!(work.len(), 1);
        arrived.push(work[0].3["text"].as_str().unwrap().to_owned());
        reloaded.settle_delivery(
            &work[0].0,
            &work[0].1,
            &work[0].2,
            Ok(json!({"status":"accepted"})),
        );
    }
    assert_eq!(
        arrived,
        ["two", "three"],
        "what the agent had is not sent again, the rest follows in order"
    );
    assert!(reloaded.pending_delivery().is_empty());
}

#[test]
fn a_message_fetched_by_a_pull_and_not_acknowledged_comes_back_after_a_restart() {
    let (directory, room, _) = pull_room(&["connector"]);
    room.register(
        "connector",
        &json!({"thread":"t","harness":"codex","capabilities":{"deliver":"unsupported"}}),
    )
    .unwrap();
    let (events, _received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "t").unwrap();
    say(&room, &sid, "one");
    say(&room, &sid, "two");
    let binding = room
        .inner
        .lock()
        .unwrap()
        .bindings
        .delivery_target("t")
        .unwrap()
        .id
        .clone();
    let fetched = room
        .pull_input(
            "connector",
            &json!({"binding_id":binding,"operation":"get"}),
        )
        .unwrap();
    assert_eq!(fetched["messages"].as_array().unwrap().len(), 2);
    // The core goes down before the connector acknowledged either.
    drop(room);
    let reloaded = crate::control::room::Room::load(
        crate::storage::PrivateDir::open(directory.path().join("private")).unwrap(),
    )
    .unwrap();
    let (requests, _receiver) = mpsc::channel(4);
    let (stop, _stopped) = tokio::sync::watch::channel(false);
    reloaded.attach(
        "connector",
        crate::control::room::ConnectorPeer {
            generation: crate::control::room::util::id(),
            sender: requests,
            stop,
        },
    );
    let again = reloaded
        .register(
            "connector",
            &json!({"thread":"t","harness":"codex","capabilities":{"deliver":"unsupported"}}),
        )
        .unwrap();
    let again = reloaded
        .pull_input(
            "connector",
            &json!({"binding_id":again["binding_id"],"operation":"get"}),
        )
        .unwrap();
    let texts: Vec<&str> = again["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        ["one", "two"],
        "offered again, in order, with the same ids"
    );
}

#[test]
fn input_the_room_cannot_keep_is_refused_and_leaves_nothing_behind() {
    use crate::control::room::tests::support::BeginAtFocus;
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    // The record cannot be written: its directory is gone.
    std::fs::remove_dir_all(directory.path().join("private")).unwrap();
    let turn = room.begin_at_focus(&sid, "lost").unwrap();
    let refused = room.finish_turn(
        &sid,
        &turn.turn_id,
        Some("kept nowhere"),
        &serde_json::Value::Null,
        crate::control::room::Continuation::Standalone,
    );
    assert!(
        refused.is_err(),
        "a message that cannot be kept is not taken"
    );
    assert_eq!(
        room.inner
            .lock()
            .unwrap()
            .journal
            .waiting_user_input()
            .count(),
        0,
        "no phantom row that is neither delivered nor kept"
    );
}
