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
fn a_conversation_that_cannot_take_injection_keeps_the_message_for_get_messages() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    let work = offered(&room);
    let (bid, rid, generation, _) = &work[0];
    // The harness has no way to put it in right now (no route, or the chat left its session); the conversation is
    // still there.
    room.settle_delivery(
        bid,
        rid,
        generation,
        Ok(json!({"status":"unsupported","detail":"t has no delivery route"})),
    );
    assert_eq!(
        room.inner.lock().unwrap().journal.find(rid).unwrap().status,
        "pending",
        "not failed while its conversation exists"
    );
    // The agent can read it with get_messages meanwhile.
    let fetched = room
        .pull_input("connector", &json!({"binding_id":bid,"operation":"get"}))
        .unwrap();
    assert_eq!(fetched["messages"][0]["text"], "one");
}

#[test]
fn a_message_answered_unsupported_is_offered_again_on_the_back_off() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    say(&room, &sid, "two");
    let work = offered(&room);
    room.settle_delivery(
        &work[0].0,
        &work[0].1,
        &work[0].2,
        Ok(json!({"status":"unsupported"})),
    );
    assert!(room.pending_delivery().is_empty(), "it waits its back-off");
    due(&room);
    let again = offered(&room);
    assert_eq!(again.len(), 1);
    assert_eq!(
        again[0].3["text"], "one",
        "the same message, and the later one waits behind it"
    );
    room.settle_delivery(
        &again[0].0,
        &again[0].1,
        &again[0].2,
        Ok(json!({"status":"accepted"})),
    );
    assert_eq!(offered(&room)[0].3["text"], "two");
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
    assert!(
        reloaded.inner.lock().unwrap().journal.last_seq() >= 3,
        "the sequence goes on, so a pull cursor held from before still precedes what is restored"
    );
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
fn input_the_room_cannot_write_down_is_still_delivered_and_written_when_it_can() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    // The record cannot be written: its directory is gone.
    let private = directory.path().join("private");
    std::fs::rename(&private, directory.path().join("away")).unwrap();
    say(&room, &sid, "still goes");
    let work = offered(&room);
    assert_eq!(
        work.len(),
        1,
        "the disk failing does not silence the person"
    );
    assert_eq!(work[0].3["text"], "still goes");
    // The disk is back: the pump's next write keeps it.
    std::fs::rename(directory.path().join("away"), &private).unwrap();
    room.persist_pending();
    let written = std::fs::read_to_string(private.join("pending-input.json")).unwrap();
    assert!(written.contains("still goes"));
}

#[test]
fn a_pull_does_not_pass_a_message_still_being_pushed() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    say(&room, &sid, "two");
    // "one" is in flight to the harness; the binding is then read by a pull.
    let work = offered(&room);
    assert_eq!(work[0].3["text"], "one");
    let fetched = room
        .pull_input(
            "connector",
            &json!({"binding_id":work[0].0,"operation":"get"}),
        )
        .unwrap();
    assert!(
        fetched["messages"].as_array().unwrap().is_empty(),
        "\"two\" does not go before \"one\" is settled"
    );
}

/// Reloads the room from its directory, as a restarted core, with a live push binding for thread `t`.
fn restarted(
    directory: &tempfile::TempDir,
    room: crate::control::room::Room,
) -> crate::control::room::Room {
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
    reloaded
}

#[test]
fn the_garage_with_a_restart_a_new_session_and_a_resend_of_the_same_client_message() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    let bid = room.snapshot(Some(&sid))["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Two typed messages said in the garage: the harness never answers.
    for (id, text) in [("c-1", "one"), ("c-2", "two")] {
        room.send_text(text, &sid, "t", &bid, id).unwrap();
    }
    let work = offered(&room);
    room.settle_delivery(
        &work[0].0,
        &work[0].1,
        &work[0].2,
        Ok(json!({"status":"failed"})),
    );
    // The core restarts; the page, back on a new session, says both again: it never saw them acknowledged.
    let reloaded = restarted(&directory, room);
    let (events, _received) = mpsc::channel(64);
    let sid = reloaded.join("device".into(), "en".into(), events).unwrap();
    reloaded.select(&sid, "t").unwrap();
    let bid = reloaded.snapshot(Some(&sid))["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (id, text) in [("c-1", "one"), ("c-2", "two")] {
        let again = reloaded.send_text(text, &sid, "t", &bid, id).unwrap();
        assert_eq!(
            again["accepted"], true,
            "said again, it is the same message"
        );
    }
    let waiting = reloaded
        .inner
        .lock()
        .unwrap()
        .journal
        .waiting_user_input()
        .count();
    assert_eq!(waiting, 2, "not queued a second time");
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
    assert_eq!(arrived, ["one", "two"], "each once, in order");
    assert!(reloaded.pending_delivery().is_empty());
}

#[test]
fn nothing_is_acknowledged_until_it_is_kept_for_a_restart() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    assert!(room.ensure_durable());
    let private = directory.path().join("private");
    std::fs::rename(&private, directory.path().join("away")).unwrap();
    say(&room, &sid, "said while the disk fails");
    assert!(!room.ensure_durable(), "the page is not told it was taken");
    assert_eq!(offered(&room).len(), 1, "it is delivered all the same");
    std::fs::rename(directory.path().join("away"), &private).unwrap();
    assert!(
        room.ensure_durable(),
        "kept at last: the page's next try is acknowledged"
    );
}

#[test]
fn a_message_fetched_by_a_pull_holds_the_later_ones_from_a_push() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    let bid = room
        .inner
        .lock()
        .unwrap()
        .bindings
        .delivery_target("t")
        .unwrap()
        .id
        .clone();
    // "one" is fetched by a pull whose answer is lost; "two" is said after it.
    let fetched = room
        .pull_input("connector", &json!({"binding_id":bid,"operation":"get"}))
        .unwrap();
    assert_eq!(fetched["messages"].as_array().unwrap().len(), 1);
    say(&room, &sid, "two");
    assert!(
        room.pending_delivery().is_empty(),
        "\"two\" is not pushed before \"one\" is acknowledged"
    );
}

/// Makes every message awaiting its receipt due for a question now.
fn ask_now(room: &crate::control::room::Room) {
    let mut inner = room.inner.lock().unwrap();
    for row in inner.journal.awaiting_receipt() {
        row.next_attempt = 0;
    }
}

fn status_of(room: &crate::control::room::Room, text: &str) -> String {
    room.inner
        .lock()
        .unwrap()
        .journal
        .find_input(text)
        .unwrap()
        .status
        .clone()
}

#[test]
fn a_message_the_agent_took_is_asked_about_in_one_batch_until_received_and_never_offered_again() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    for words in ["one", "two"] {
        say(&room, &sid, words);
        let work = offered(&room);
        assert_eq!(work.len(), 1);
        room.settle_delivery(
            &work[0].0,
            &work[0].1,
            &work[0].2,
            Ok(json!({"status":"accepted"})),
        );
    }
    assert_eq!(status_of(&room, "one"), "delivered");
    ask_now(&room);
    assert!(
        room.pending_delivery().is_empty(),
        "phase 2 never re-offers"
    );
    let questions = room.pending_receipts();
    assert_eq!(questions.len(), 1, "one question per conversation");
    let ids = questions[0].message_ids.clone();
    assert_eq!(ids.len(), 2);
    assert!(
        room.pending_receipts().is_empty(),
        "not asked again while the question waits"
    );
    // The agent has "one"; "two" is not known yet.
    room.settle_receipts(
        &questions[0].binding_id,
        &questions[0].peer.generation,
        &ids,
        Ok(json!({"received":[ids[0]]})),
    );
    assert_eq!(status_of(&room, "one"), "read");
    assert_eq!(status_of(&room, "two"), "delivered");
    // A lost answer is recovered by the next question.
    ask_now(&room);
    let again = room.pending_receipts();
    assert_eq!(again[0].message_ids, [ids[1].clone()]);
    room.settle_receipts(
        &again[0].binding_id,
        &again[0].peer.generation,
        &again[0].message_ids,
        Err(crate::control::room::PeerError),
    );
    assert_eq!(status_of(&room, "two"), "delivered");
    ask_now(&room);
    let last = room.pending_receipts();
    room.settle_receipts(
        &last[0].binding_id,
        &last[0].peer.generation,
        &last[0].message_ids,
        Ok(json!({"received":[ids[1]]})),
    );
    assert_eq!(status_of(&room, "two"), "read");
    ask_now(&room);
    assert!(room.pending_receipts().is_empty());
}

#[test]
fn a_message_the_connector_has_no_record_of_is_offered_again() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    let work = offered(&room);
    room.settle_delivery(
        &work[0].0,
        &work[0].1,
        &work[0].2,
        Ok(json!({"status":"accepted"})),
    );
    ask_now(&room);
    let question = room.pending_receipts().remove(0);
    room.settle_receipts(
        &question.binding_id,
        &question.peer.generation,
        &question.message_ids,
        Ok(json!({"received":[],"unknown":question.message_ids})),
    );
    let again = offered(&room);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].3["message_id"], work[0].3["message_id"]);
}

#[test]
fn a_delivery_answer_that_says_received_settles_both_phases_at_once() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    let work = offered(&room);
    room.settle_delivery(
        &work[0].0,
        &work[0].1,
        &work[0].2,
        Ok(json!({"status":"accepted","received":true})),
    );
    assert_eq!(status_of(&room, "one"), "read");
}

#[test]
fn an_older_connectors_unknown_is_handed_over_not_final() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    say(&room, &sid, "two");
    let work = offered(&room);
    room.settle_delivery(
        &work[0].0,
        &work[0].1,
        &work[0].2,
        Ok(json!({"status":"unknown"})),
    );
    assert_eq!(status_of(&room, "one"), "delivered");
    assert_eq!(offered(&room)[0].3["text"], "two", "the next one follows");
}

#[test]
fn a_restart_keeps_each_message_in_its_phase() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    for words in ["taken", "in flight", "waiting"] {
        say(&room, &sid, words);
    }
    let work = offered(&room);
    room.settle_delivery(
        &work[0].0,
        &work[0].1,
        &work[0].2,
        Ok(json!({"status":"accepted"})),
    );
    let in_flight = offered(&room);
    assert_eq!(in_flight[0].3["text"], "in flight");
    let reloaded = restarted(&directory, room);
    assert_eq!(status_of(&reloaded, "taken"), "delivered");
    assert_eq!(status_of(&reloaded, "in flight"), "pending");
    // The one being sent is offered again (the connector never hands an id over twice); the taken one never is.
    let again = offered(&reloaded);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].3["text"], "in flight");
    assert_eq!(again[0].3["message_id"], in_flight[0].3["message_id"]);
    ask_now(&reloaded);
    let question = reloaded.pending_receipts();
    assert_eq!(question.len(), 1);
    assert_eq!(
        question[0].message_ids,
        [work[0].3["message_id"].as_str().unwrap().to_owned()]
    );
}

#[test]
fn get_messages_counts_what_waits_behind_a_push_in_flight() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    say(&room, &sid, "two");
    let work = offered(&room);
    let fetched = room
        .pull_input(
            "connector",
            &json!({"binding_id":work[0].0,"operation":"get"}),
        )
        .unwrap();
    assert!(fetched["messages"].as_array().unwrap().is_empty());
    assert_eq!(fetched["count"], 2, "on its way, not nothing");
    assert_eq!(fetched["pending"], true);
}

#[test]
fn why_an_offer_was_not_taken_is_shown_only_while_the_message_waits() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    say(&room, &sid, "one");
    let work = offered(&room);
    room.settle_delivery(
        &work[0].0,
        &work[0].1,
        &work[0].2,
        Ok(json!({"status":"unsupported"})),
    );
    let reason = |room: &crate::control::room::Room| {
        room.history(Some("t"))["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["text"] == "one")
            .unwrap()["audio_reason"]
            .clone()
    };
    assert_eq!(reason(&room), "unsupported");
    room.pull_input(
        "connector",
        &json!({"binding_id":work[0].0,"operation":"get"}),
    )
    .unwrap();
    assert_eq!(
        reason(&room),
        serde_json::Value::Null,
        "fetched: no longer waiting"
    );
}

#[test]
fn a_note_nobody_can_take_is_never_given_up() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    // A reply never heard, then a return: one user_returned note.
    let revision = room
        .inner
        .lock()
        .unwrap()
        .browsers
        .get(&sid)
        .unwrap()
        .revision;
    room.publish(
        &json!({"session_id":sid,"thread_id":"t","revision":revision,"epoch":0,
        "utterance_id":"r","text":"Missed"}),
    );
    room.park(&sid, true);
    room.resume(&sid, &[]);
    room.inner
        .lock()
        .unwrap()
        .unheard
        .note_mut("t")
        .unwrap()
        .due = 0;
    for _ in 0..12 {
        let work = room.pending_delivery();
        assert_eq!(work.len(), 1, "still offered");
        assert_eq!(work[0].3["channel"], "note");
        let (bid, rid, peer, _) = &work[0];
        room.settle_delivery(
            bid,
            rid,
            &peer.generation,
            Ok(json!({"status":"unsupported"})),
        );
        let mut inner = room.inner.lock().unwrap();
        let row = inner.journal.find_mut(rid).unwrap();
        assert_eq!(row.status, "pending");
        row.next_attempt = 0;
    }
}

#[test]
fn a_typed_message_carries_the_block_and_no_return_note_follows() {
    let (directory, room, _) = pull_room(&["connector"]);
    let sid = garage(&directory, &room);
    let revision = room
        .inner
        .lock()
        .unwrap()
        .browsers
        .get(&sid)
        .unwrap()
        .revision;
    room.publish(
        &json!({"session_id":sid,"thread_id":"t","revision":revision,"epoch":0,
        "utterance_id":"r","text":"Missed reply"}),
    );
    room.park(&sid, true);
    room.resume(&sid, &[]);
    let bid = room.snapshot(Some(&sid))["binding"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    room.send_text("Typed back", &sid, "t", &bid, "typed-1")
        .unwrap();
    assert!(room.inner.lock().unwrap().unheard.note_mut("t").is_none());
    let work = offered(&room);
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].3["text"], "Typed back");
    let payload = room
        .inner
        .lock()
        .unwrap()
        .journal
        .find_input("Typed back")
        .unwrap()
        .payload
        .clone()
        .unwrap();
    assert_eq!(payload["unheard"]["count"], 1);
}
