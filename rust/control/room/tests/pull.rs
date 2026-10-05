use serde_json::json;
use tokio::sync::mpsc;

use super::support::{pull, pull_room, say};
use crate::control::room::pull::PULL_PAGE;

#[test]
fn pull_reads_only_its_live_binding_and_keeps_messages_until_ack() {
    let (_directory, room, _) = pull_room(&["pull-owner", "other"]);
    let joined = room
        .register(
            "pull-owner",
            &json!({"thread":"pull-thread","harness":"codex","input_mode":"pull"}),
        )
        .unwrap();
    let bid = joined["binding_id"].as_str().unwrap().to_owned();
    let (events, _received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "pull-thread").unwrap();
    say(&room, &sid, "first");
    say(&room, &sid, "second");
    assert!(
        room.pending_delivery().is_empty(),
        "pull input must not also use push"
    );
    let check = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"check"}),
    )
    .unwrap();
    assert_eq!(
        (check["pending"].clone(), check["count"].clone()),
        (json!(true), json!(2))
    );
    assert_eq!(check["messages"].as_array().unwrap().len(), 0);
    assert_eq!(check["fresh"], 2);
    assert_eq!(
        room.history(Some("pull-thread"))["messages"][0]["status"],
        "pending"
    );
    assert!(pull(&room, "other", json!({"binding_id":bid,"operation":"get"})).is_err());
    assert!(pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"check","ack_ids":[]})
    )
    .is_err());
    assert!(pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get","ack_ids":[""]})
    )
    .is_err());

    let first = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get"}),
    )
    .unwrap();
    let texts: Vec<&str> = first["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["first", "second"]);
    assert_eq!(first["more"], false);
    assert_eq!(
        room.history(Some("pull-thread"))["messages"][0]["status"],
        "delivered"
    );
    let one = first["messages"][0]["message_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let two = first["messages"][1]["message_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // A failed or lost fetch is retried: the same IDs come back until acknowledged.
    let again = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get"}),
    )
    .unwrap();
    assert_eq!(again["messages"][0]["message_id"], one.as_str());
    assert_eq!(again["messages"][1]["message_id"], two.as_str());
    let held = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"check"}),
    )
    .unwrap();
    assert_eq!(
        (held["count"].clone(), held["fresh"].clone()),
        (json!(2), json!(0))
    );
    let past = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get","after":first["cursor"]}),
    )
    .unwrap();
    assert_eq!(past["messages"].as_array().unwrap().len(), 0);
    assert_eq!(past["count"], 2);

    // An ID this binding does not hold changes nothing.
    let foreign = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get","ack_ids":["somebody-else"]}),
    )
    .unwrap();
    assert_eq!(
        (foreign["acknowledged"].clone(), foreign["count"].clone()),
        (json!(0), json!(2))
    );

    let acked = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get","ack_ids":[one]}),
    )
    .unwrap();
    assert_eq!(
        (acked["acknowledged"].clone(), acked["count"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(acked["messages"][0]["message_id"], two.as_str());
    assert_eq!(
        room.history(Some("pull-thread"))["messages"][0]["status"],
        "read"
    );

    // Repeating an acknowledgement whose answer was lost is harmless.
    let repeated = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get","ack_ids":[one, two]}),
    )
    .unwrap();
    assert_eq!(
        (
            repeated["acknowledged"].clone(),
            repeated["count"].clone(),
            repeated["pending"].clone()
        ),
        (json!(1), json!(0), json!(false))
    );
    room.unregister("pull-owner", &bid);
    assert!(pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"check"})
    )
    .is_err());
}

#[test]
fn pull_pages_without_silent_loss() {
    let (_directory, room, _) = pull_room(&["pull-owner"]);
    let bid = room
        .register(
            "pull-owner",
            &json!({"thread":"pull-thread","harness":"codex","input_mode":"pull"}),
        )
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (events, _received) = mpsc::channel(256);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "pull-thread").unwrap();
    for n in 0..(PULL_PAGE + 3) {
        say(&room, &sid, &format!("message {n}"));
    }
    let page = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get"}),
    )
    .unwrap();
    assert_eq!(page["messages"].as_array().unwrap().len(), PULL_PAGE);
    assert_eq!(
        (page["more"].clone(), page["count"].clone()),
        (json!(true), json!(PULL_PAGE + 3))
    );
    let rest = pull(
        &room,
        "pull-owner",
        json!({"binding_id":bid,"operation":"get","after":page["cursor"]}),
    )
    .unwrap();
    let texts: Vec<&str> = rest["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    let expected: Vec<String> = (PULL_PAGE..PULL_PAGE + 3)
        .map(|n| format!("message {n}"))
        .collect();
    assert_eq!(texts, expected);
    assert_eq!(rest["more"], false);
}

#[test]
fn pull_claims_return_to_push_when_the_pull_binding_goes_away() {
    let (_directory, room, generations) = pull_room(&["pusher", "puller"]);
    let push_bid = room
        .register("pusher", &json!({"thread":"shared","harness":"claude"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (events, _received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "shared").unwrap();
    // Make the pull binding the newest one on the thread.
    room.inner
        .lock()
        .unwrap()
        .bindings
        .get_mut(&push_bid)
        .unwrap()
        .created -= 10;
    let pull_bid = room
        .register(
            "puller",
            &json!({"thread":"shared","harness":"codex","input_mode":"pull"}),
        )
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    say(&room, &sid, "held");
    assert!(
        room.pending_delivery().is_empty(),
        "the newest binding pulls"
    );
    // The older push binding may not read the thread's input by pull either.
    assert!(pull(
        &room,
        "pusher",
        json!({"binding_id":push_bid,"operation":"check"})
    )
    .is_err());
    let got = pull(
        &room,
        "puller",
        json!({"binding_id":pull_bid,"operation":"get"}),
    )
    .unwrap();
    assert_eq!(got["messages"][0]["text"], "held");

    // The pull connector drops: its unacknowledged message returns to the push binding.
    room.detach("puller", &generations[1]);
    let delivery = room.pending_delivery();
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0].0, push_bid);
    assert_eq!(delivery[0].3["text"], "held");
    assert_eq!(
        delivery[0].3["message_id"],
        got["messages"][0]["message_id"]
    );
}

#[test]
fn re_registering_as_push_releases_pull_claims_once() {
    let (_directory, room, _) = pull_room(&["connector"]);
    let joined = |mode: &str| {
        room.register(
            "connector",
            &json!({"thread":"switch","harness":"codex","input_mode":mode}),
        )
        .unwrap()["binding_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let bid = joined("pull");
    let (events, _received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, "switch").unwrap();
    say(&room, &sid, "switching");
    pull(
        &room,
        "connector",
        json!({"binding_id":bid,"operation":"get"}),
    )
    .unwrap();
    // Re-registering in pull mode keeps the claim; the message is not pushed.
    assert_eq!(joined("pull"), bid);
    assert!(room.pending_delivery().is_empty());
    assert_eq!(
        pull(
            &room,
            "connector",
            json!({"binding_id":bid,"operation":"check"})
        )
        .unwrap()["count"],
        1
    );
    // Switching the same binding to push hands the unacknowledged message to push, once.
    assert_eq!(joined("push"), bid);
    assert!(pull(
        &room,
        "connector",
        json!({"binding_id":bid,"operation":"check"})
    )
    .is_err());
    let delivery = room.pending_delivery();
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0].3["text"], "switching");
    assert!(room.pending_delivery().is_empty());
}
