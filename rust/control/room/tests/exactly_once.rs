use serde_json::json;
use std::thread;
use tokio::sync::mpsc;

use super::support::{pull, pull_room, say};

fn fixture(inject: bool) -> (tempfile::TempDir, crate::control::room::Room, String) {
    let (directory, room, _) = pull_room(&["connector"]);
    let bid = room
        .register(
            "connector",
            &json!({"thread":"t","harness":"cursor","capabilities":{
                "deliver":if inject {"supported"} else {"unsupported"}}}),
        )
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (events, _) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "t").unwrap();
    say(&room, &sid, "hello");
    (directory, room, bid)
}

fn get(room: &crate::control::room::Room, bid: &str) -> serde_json::Value {
    pull(
        room,
        "connector",
        json!({"binding_id":bid,"operation":"get"}),
    )
    .unwrap()
}

#[test]
fn accepted_injection_is_absent_from_get_messages() {
    let (_, room, bid) = fixture(true);
    let delivery = room.pending_delivery();
    assert_eq!(delivery.len(), 1);
    let (target, rid, peer, _) = &delivery[0];
    room.settle_delivery(
        target,
        rid,
        &peer.generation,
        Ok(json!({"status":"accepted"})),
    );
    assert!(get(&room, &bid)["messages"].as_array().unwrap().is_empty());
}

#[test]
fn failed_or_unavailable_injection_is_claimed_once_then_acknowledged() {
    for inject in [false, true] {
        let (_, room, bid) = fixture(inject);
        if inject {
            let delivery = room.pending_delivery();
            let (target, rid, peer, _) = &delivery[0];
            room.settle_delivery(
                target,
                rid,
                &peer.generation,
                Ok(json!({"status":"failed"})),
            );
        } else {
            assert!(room.pending_delivery().is_empty());
        }
        let claimed = get(&room, &bid);
        assert_eq!(claimed["messages"].as_array().unwrap().len(), 1);
        assert!(room.pending_delivery().is_empty());
        let id = claimed["messages"][0]["message_id"].clone();
        let acked = pull(
            &room,
            "connector",
            json!({"binding_id":bid,"operation":"get","ack_ids":[id]}),
        )
        .unwrap();
        assert_eq!(acked["acknowledged"], 1);
        assert!(acked["messages"].as_array().unwrap().is_empty());
    }
}

#[test]
fn concurrent_injection_and_get_claim_one_journal_state() {
    for _ in 0..100 {
        let (_, room, bid) = fixture(true);
        let (delivery, claimed) = thread::scope(|scope| {
            let injection = scope.spawn(|| room.pending_delivery());
            let fetch = scope.spawn(|| get(&room, &bid));
            (injection.join().unwrap(), fetch.join().unwrap())
        });
        let pushed = delivery.len();
        let pulled = claimed["messages"].as_array().unwrap().len();
        assert_eq!(pushed + pulled, 1);
        if pushed == 1 {
            let (target, rid, peer, _) = &delivery[0];
            room.settle_delivery(
                target,
                rid,
                &peer.generation,
                Ok(json!({"status":"accepted"})),
            );
            assert!(get(&room, &bid)["messages"].as_array().unwrap().is_empty());
        } else {
            assert!(room.pending_delivery().is_empty());
        }
    }
}
