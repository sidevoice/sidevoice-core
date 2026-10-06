//! The connector's two links to the core, driven from the connector's side of the wire: Socket.IO v2 and
//! JSON-RPC v3, both on the local socket only. A conversation registers a binding, a browser focuses it and types,
//! the input is delivered and acknowledged, the receipts reach the browser, and the conversation's replies are
//! played in order — across a reconnect, a replaced connection and a saturated one.

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use support::*;
use tokio_tungstenite::tungstenite::Message;

const THREAD: &str = "typed-thread";

#[tokio::test(flavor = "multi_thread")]
async fn refused_and_turned_away_calls_give_their_seat_back() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let _first = core.join(&token, Value::Null).await;
    core.calls_become(1).await;
    for _ in 0..4 {
        // A first message that is not a hello.
        let mut refused = Browser::new(core.open_call(&token).await);
        refused.send_raw(Message::binary(b"invalid hello".to_vec()));
        refused.closed(STEP).await;
        core.calls_become(1).await;
        // A hello asking for a stage this machine cannot run.
        let mut refused = Browser::new(core.open_call(&token).await);
        refused
            .send(
                "voice-hello",
                json!({"settings": {"tts": {"place": "host", "model": "kokoro-82m-v1.0"}}}),
            )
            .await;
        assert_eq!(
            refused.frame("error").await["key"],
            "place_host_unavailable"
        );
        assert_eq!(refused.closed(STEP).await, 1008);
        core.calls_become(1).await;
    }
    let mut admitted = Vec::new();
    for _ in 0..7 {
        admitted.push(core.join(&token, Value::Null).await);
    }
    core.calls_become(8).await;
    for _ in 0..4 {
        let mut refused = Browser::new(core.open_call(&token).await);
        refused.send("voice-hello", json!({})).await;
        assert_eq!(refused.frame("error").await["reason"], "room_is_full");
        assert_eq!(refused.closed(STEP).await, 1013);
        core.calls_become(8).await;
    }
    for browser in admitted {
        browser.close().await;
    }
    core.calls_become(1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_input_is_delivered_read_and_answered_and_replies_play_in_order() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (mut peer, binding) = v2_with_binding(&core, "typed", THREAD).await;
    assert_eq!(core.participant(&token, THREAD).await["thread_id"], THREAD);
    let focus = core.select(&token, &session, THREAD).await;
    let revision = core.revision(&token, &session).await;

    let message = message_id();
    let sent = core
        .text(
            &token,
            (&session, THREAD, &focus),
            "Hello from the browser",
            &message,
        )
        .await;
    assert_eq!(sent["revision"], revision);
    assert_eq!(browser.receipt("pending").await["history_id"], sent["id"]);
    let delivery = accept_delivery(&mut peer).await;
    assert_eq!(delivery.data["binding_id"], binding);
    assert_eq!(browser.receipt("delivered").await["history_id"], sent["id"]);
    peer.emit(
        "input.read",
        json!({"binding_id": binding, "message_id": message}),
    );
    assert_eq!(browser.receipt("read").await["history_id"], sent["id"]);
    peer.emit(
        "input.working",
        json!({"binding_id": binding, "working": true}),
    );
    assert_eq!(browser.frame("voice-conversation").await["working"], true);

    let publish = |utterance: &str, session: &str, revision: u64, text: &str| {
        json!({"event_id": format!("event-{utterance}"), "utterance_id": utterance, "binding_id": binding,
            "session_id": session, "revision": revision, "text": text, "language": "en"})
    };
    let answer = peer
        .call(
            "speech.publish",
            publish("reply", &session, revision, "Reply from the connector"),
        )
        .await
        .data;
    assert_eq!(
        (answer["status"].as_str(), answer["text_saved"].as_bool()),
        (Some("queued"), Some(true))
    );
    let speech = browser.frame("voice-speech").await;
    assert_eq!(speech["text"], "Reply from the connector");
    assert_eq!(speech["session_id"], session.as_str());
    let spoken = speech["revision"].as_u64().unwrap();
    core.played(&token, &session, "reply", spoken).await;
    assert_eq!(
        core.receipt(&token, &session, "reply", spoken, "playing")
            .await,
        409
    );
    let history = core.history(&token, THREAD).await;
    let rows: Vec<_> = history
        .iter()
        .map(|row| (row["seq"].clone(), row["status"].clone()))
        .collect();
    assert_eq!(
        rows,
        [
            (json!(1), json!("read")),
            (json!(2), json!("playback_finished"))
        ],
        "{history:?}"
    );

    // One browser hears replies in order: a queued reply waits for the receipt of the one before it.
    for index in [2, 3] {
        let utterance = format!("queued-{index}");
        let answer = peer
            .call(
                "speech.publish",
                publish(
                    &utterance,
                    &session,
                    revision,
                    &format!("Queued reply {index}"),
                ),
            )
            .await
            .data;
        assert_eq!(answer["status"], "queued");
        if index == 2 {
            assert_eq!(
                browser.frame("voice-speech").await["utterance_id"],
                "queued-2"
            );
        }
    }
    browser
        .none_of(
            &["voice-speech", "voice-speech-audio"],
            Duration::from_millis(350),
        )
        .await;
    assert_eq!(
        core.receipt(&token, &session, "queued-2", revision, "playback_finished")
            .await,
        200
    );
    assert_eq!(
        browser.frame("voice-speech").await["utterance_id"],
        "queued-3"
    );
    assert_eq!(
        core.receipt(&token, &session, "queued-3", revision, "skipped")
            .await,
        200
    );
    let history = core.history(&token, THREAD).await;
    let statuses: Vec<_> = history[history.len() - 2..]
        .iter()
        .map(|row| row["status"].clone())
        .collect();
    assert_eq!(
        statuses,
        [json!("playback_finished"), json!("interrupted")],
        "{history:?}"
    );

    // Two browsers on the same conversation hear the same reply; one finishing it is enough.
    let mut second = core.join(&token, Value::Null).await;
    core.select(&token, &second.session, THREAD).await;
    let second_revision = core.revision(&token, &second.session).await;
    let answer = peer
        .call(
            "speech.publish",
            publish("both", &session, revision, "One reply for both listeners"),
        )
        .await
        .data;
    assert_eq!(answer["status"], "queued");
    assert_eq!(browser.frame("voice-speech").await["utterance_id"], "both");
    assert_eq!(second.frame("voice-speech").await["utterance_id"], "both");
    assert_eq!(
        core.receipt(&token, &session, "both", revision, "playback_finished")
            .await,
        200
    );
    assert_eq!(
        core.receipt(&token, &second.session, "both", second_revision, "skipped")
            .await,
        200
    );
    let history = core.history(&token, THREAD).await;
    assert_eq!(
        history.last().unwrap()["status"],
        "playback_finished",
        "{history:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_host_scan_holds_no_input_and_a_host_refusal_keeps_only_its_key() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (mut peer, _) = v2_with_binding(&core, "scan", THREAD).await;
    let focus = core.select(&token, &session, THREAD).await;

    let scan = tokio::spawn(core.get("/api/host/agents?rescan=1").token(&token).send());
    let ask = peer.event("agents.list").await;
    let sent = core
        .text(
            &token,
            (&session, THREAD, &focus),
            "Input during a scan",
            &message_id(),
        )
        .await;
    assert_eq!(browser.receipt("pending").await["thread_id"], THREAD);
    accept_delivery(&mut peer).await;
    let delivered = browser
        .wait("voice-input-receipt", Duration::from_millis(1200), |data| {
            data["status"] == "delivered"
        })
        .await;
    assert_eq!(delivered["history_id"], sent["id"]);
    peer.answer(
        ask.id.unwrap(),
        json!({"agents": [], "custom": {}, "scanned_at": null}),
    );
    assert_eq!(scan.await.unwrap().status, 200);

    let listing = tokio::spawn(core.get("/api/host/agents").token(&token).send());
    let ask = peer.event("agents.list").await;
    peer.answer(
        ask.id.unwrap(),
        json!({"error": {"key": "host.agent-unavailable", "message": "A message the connector rendered",
            "params": {"agent": "fixture", "raw_output": "private output"}}}),
    );
    let refused = listing.await.unwrap();
    assert_eq!(refused.status, 409, "{refused:?}");
    assert_eq!(refused.json()["error"]["key"], "host.agent-unavailable");
    assert_eq!(
        refused.json()["error"]["params"],
        json!({"agent": "fixture"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnected_connector_keeps_its_binding_and_a_replaced_one_settles_nothing() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (peer, binding) = v2_with_binding(&core, "reconnect", THREAD).await;
    let focus = core.select(&token, &session, THREAD).await;

    peer.disconnect();
    eventually(STEP, "the conversation to be unavailable", || {
        let participant = core.participant(&token, THREAD);
        async move { (participant.await["available"] == false).then_some(()) }
    })
    .await;
    core.text(
        &token,
        (&session, THREAD, &focus),
        "Held across a reconnect",
        &message_id(),
    )
    .await;
    browser.receipt("pending").await;
    let (mut second, again) = v2_with_binding(&core, "reconnect", THREAD).await;
    assert_eq!(
        again, binding,
        "the conversation keeps its binding across a reconnect"
    );
    accept_delivery(&mut second).await;
    browser.receipt("delivered").await;

    // The connection is replaced while its answer to a delivery is still pending: the input is delivered again
    // on the new connection, and the old one's late answer settles nothing.
    let late = message_id();
    core.text(
        &token,
        (&session, THREAD, &focus),
        "Late answer from a replaced connection",
        &late,
    )
    .await;
    browser.receipt("pending").await;
    let held = second.event("input.deliver").await;
    let (mut third, again) = v2_with_binding(&core, "reconnect", THREAD).await;
    assert_eq!(again, binding);
    accept_delivery(&mut third).await;
    browser.receipt("delivered").await;
    second.answer(held.id.unwrap(), json!({"status": "accepted"}));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let history = core.history(&token, THREAD).await;
    let row = history
        .iter()
        .find(|row| row["id"].as_str().is_some_and(|id| id.ends_with(&late)))
        .expect("the late input's row");
    assert_eq!(row["status"], "delivered", "{history:?}");
    third.disconnect();
    eventually(STEP, "the conversation to be unavailable", || {
        let participant = core.participant(&token, THREAD);
        async move { (participant.await["available"] == false).then_some(()) }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_v3_connector_speaks_json_rpc_on_the_local_socket_only() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    assert_eq!(core.ready["connector_protocols"], json!([2, 3]));
    assert_eq!(
        core.get("/api/connectors/v3").send().await.status,
        404,
        "TCP never carries a connector link"
    );
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();

    let mut rpc = Rpc::hello(&core).await;
    assert_eq!(
        rpc.method("connector.welcome", STEP).await["params"]["protocol"],
        3
    );
    let registered = rpc
        .request(
            "binding.register",
            json!({"client_ref": "v3", "harness": "fixture", "thread": "v3-thread",
                "title": "JSON-RPC conversation", "capabilities": {"deliver": "supported"}}),
        )
        .await;
    let binding = registered["result"]["binding_id"]
        .as_str()
        .expect("a binding")
        .to_owned();
    let focus = core.select(&token, &session, "v3-thread").await;
    let revision = core.revision(&token, &session).await;
    let sent = core
        .text(
            &token,
            (&session, "v3-thread", &focus),
            "Input over JSON-RPC",
            &message_id(),
        )
        .await;
    browser.receipt("pending").await;
    let delivery = rpc.method("input.deliver", STEP).await;
    assert_eq!(delivery["params"]["binding_id"], binding.as_str());
    rpc.answer(&delivery["id"], json!({"status": "accepted"}));
    assert_eq!(browser.receipt("delivered").await["history_id"], sent["id"]);

    let publish = |utterance: &str, revision: u64, text: &str| {
        json!({"event_id": format!("event-{utterance}"), "utterance_id": utterance, "binding_id": binding,
            "session_id": session, "revision": revision, "text": text})
    };
    let stale = rpc
        .request(
            "speech.publish",
            publish("stale", revision - 1, "A reply to a turn the focus left"),
        )
        .await;
    assert_eq!(stale["result"]["status"], "text_only", "{stale}");
    assert_eq!(stale["result"]["reason"], "focus_changed", "{stale}");
    let reply = rpc
        .request(
            "speech.publish",
            publish("current", revision, "Reply over JSON-RPC"),
        )
        .await;
    assert_eq!(reply["result"]["status"], "queued", "{reply}");
    assert_eq!(reply["result"]["text_saved"], true, "{reply}");
    assert_eq!(
        browser.frame("voice-speech").await["text"],
        "Reply over JSON-RPC"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saturated_v3_link_recovers_and_a_malformed_id_ends_it_without_losing_the_binding() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let mut rpc = Rpc::hello(&core).await;
    let register = json!({"client_ref": "raw", "harness": "fixture", "thread": "raw-thread",
        "capabilities": {"deliver": "supported"}});
    let binding = rpc.request("binding.register", register.clone()).await["result"]["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let focus = core.select(&token, &session, "raw-thread").await;

    // Every request the core may have outstanding on one link, none answered: each caller gets its timeout.
    let callers: Vec<_> = (0..128)
        .map(|_| tokio::spawn(core.get("/api/host/agents").token(&token).send()))
        .collect();
    let mut asked = Vec::new();
    for _ in 0..128 {
        asked.push(rpc.method("agents.list", Duration::from_secs(25)).await["id"].clone());
    }
    let distinct: std::collections::HashSet<String> = asked.iter().map(Value::to_string).collect();
    assert_eq!(distinct.len(), 128);
    for caller in callers {
        assert_eq!(caller.await.unwrap().status, 504);
    }
    // A late result is ignored, and the same link still delivers.
    rpc.answer(&asked[0], json!({"agents": []}));
    let sent = core
        .text(
            &token,
            (&session, "raw-thread", &focus),
            "Recovered on the same link",
            &message_id(),
        )
        .await;
    browser.receipt("pending").await;
    let delivery = rpc.method("input.deliver", STEP).await;
    assert_eq!(delivery["params"]["binding_id"], binding.as_str());
    rpc.answer(&delivery["id"], json!({"status": "accepted"}));
    assert_eq!(browser.receipt("delivered").await["history_id"], sent["id"]);

    // The minimum signed 64-bit integer is not a JSON-RPC id this link accepts.
    rpc.send_text(
        r#"{"jsonrpc":"2.0","id":-9223372036854775808,"method":"input.read","params":{}}"#,
    );
    assert_eq!(rpc.closed(STEP).await, 1002);
    let connector = core.connector_id();
    eventually(STEP, "the connector to be listed as disconnected", || {
        let listing = core.get("/api/connectors").token(&token).send();
        let connector = connector.clone();
        async move {
            let listing = listing.await.json();
            let row = listing["connectors"]
                .as_array()?
                .iter()
                .find(|row| row["id"] == connector.as_str())?
                .clone();
            (row["connected"] == false).then_some(())
        }
    })
    .await;

    let mut rpc = Rpc::hello(&core).await;
    let mut again = register;
    again["binding_id"] = json!(binding);
    let reattached = rpc.request("binding.register", again).await;
    assert_eq!(reattached["result"]["binding_id"], binding.as_str());
    let sent = core
        .text(
            &token,
            (&session, "raw-thread", &focus),
            "Delivered after a malformed id",
            &message_id(),
        )
        .await;
    browser.receipt("pending").await;
    let delivery = rpc.method("input.deliver", STEP).await;
    rpc.answer(&delivery["id"], json!({"status": "accepted"}));
    assert_eq!(browser.receipt("delivered").await["history_id"], sent["id"]);
}
