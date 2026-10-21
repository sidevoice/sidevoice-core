//! The connector's link to the core, driven from the connector's side of the wire: JSON-RPC v3, on the local
//! socket only. A conversation registers a binding, a browser focuses it and types,
//! the input is delivered and acknowledged, the receipts reach the browser, and the conversation's replies reach
//! the browser as text with how far each was heard — across a reconnect, a replaced connection and a saturated one.

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use support::*;
use tokio_tungstenite::tungstenite::Message;

const THREAD: &str = "typed-thread";

#[tokio::test(flavor = "multi_thread")]
async fn uncertain_speech_recovery_over_rpc_saves_without_audio_and_recovers_prior_answer() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (mut peer, binding) = connector_with_binding(&core, "recovery", THREAD).await;
    core.select(&token, &session, THREAD).await;
    let revision = core.revision(&token, &session).await;

    let admitted = json!({"event_id":"known-event","utterance_id":"known","binding_id":binding,
        "session_id":session,"revision":revision,"epoch":0,"text":"Known reply"});
    let original = peer.call("speech.publish", admitted.clone()).await;
    assert_eq!(original["status"], "queued");
    assert_eq!(browser.frame("voice-reply").await["utterance_id"], "known");
    let mut recover_known = admitted;
    recover_known["recovery_binding_id"] = json!(binding);
    assert_eq!(peer.call("speech.recover", recover_known).await, original);

    let missing = json!({"event_id":"lost-event","utterance_id":"lost","binding_id":binding,
        "recovery_binding_id":binding,"session_id":"old-session","revision":revision,
        "epoch":0,"text":"Lost request"});
    let recovered = peer.call("speech.recover", missing.clone()).await;
    assert_eq!(recovered["status"], "text_only");
    assert!(recovered.get("reason").is_none());
    assert_eq!(recovered["text_saved"], true);
    assert_eq!(recovered["event_id"], "lost-event");
    assert_eq!(peer.call("speech.recover", missing).await, recovered);
    browser
        .hearing("old-session:voice:lost", "lost", "heard", None)
        .await;
    assert_eq!(
        core.history(&token, THREAD)
            .await
            .iter()
            .filter(|r| r["role"] == "assistant")
            .count(),
        2
    );

    browser.start_turn("fresh-turn").await;
    browser
        .report(
            "voice-user-turn",
            json!({"phase":"finished","turn_id":"fresh-turn","text":"Next"}),
        )
        .await;
    let fresh = peer.call("speech.publish", json!({"event_id":"fresh-event","utterance_id":"fresh",
        "binding_id":binding,"session_id":session,"revision":revision,"epoch":1,"text":"Fresh reply"})).await;
    assert_eq!(fresh["status"], "queued", "{fresh}");
    assert_eq!(browser.frame("voice-reply").await["utterance_id"], "fresh");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_turn_refusal_keeps_rpc_identity_and_empty_notice_reaches_pull() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let mut peer = Rpc::hello(&core).await;
    let registered = peer
        .call(
            "binding.register",
            json!({"client_ref":"pull-turn","harness":"fixture","thread":"turn-thread",
                "capabilities":{"deliver":"unsupported"}}),
        )
        .await;
    let binding = registered["binding_id"].as_str().unwrap();
    core.select(&token, &session, "turn-thread").await;
    let revision = core.revision(&token, &session).await;
    browser.start_turn("confirmed-turn").await;
    let speech = json!({"event_id":"event-refused","utterance_id":"refused",
        "binding_id":binding,"session_id":session,"revision":revision,"epoch":1,
        "text":"Attempted while the person speaks"});
    let refused = peer.call("speech.publish", speech.clone()).await;
    assert_eq!(refused["status"], "rejected", "{refused}");
    assert_eq!(refused["reason"], "user_speaking");
    assert_eq!(refused["event_id"], "event-refused");
    assert_eq!(refused["utterance_id"], "refused");

    browser
        .report(
            "voice-user-turn",
            json!({"phase":"finished","turn_id":"confirmed-turn","text":""}),
        )
        .await;
    let retry = peer.call("speech.publish", speech).await;
    assert_eq!(retry, refused);
    let first = peer
        .call(
            "input.pull",
            json!({"binding_id":binding,"operation":"get"}),
        )
        .await;
    assert_eq!(first["messages"][0]["channel"], "note", "{first}");
    assert_eq!(first["messages"][0]["text"], "");
    assert_eq!(first["messages"][0]["notice"]["kind"], "turn_empty");
    let message_id = first["messages"][0]["message_id"].clone();
    let again = peer
        .call(
            "input.pull",
            json!({"binding_id":binding,"operation":"get"}),
        )
        .await;
    assert_eq!(again["messages"][0]["message_id"], message_id);
    let acked = peer
        .call(
            "input.pull",
            json!({"binding_id":binding,"operation":"get",
            "ack_ids":[message_id]}),
        )
        .await;
    assert_eq!(acked["acknowledged"], 1);
    assert_eq!(acked["count"], 0);
    assert!(core
        .history(&token, "turn-thread")
        .await
        .iter()
        .all(|row| row["role"] != "assistant"));
}

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
    }
    let mut admitted = Vec::new();
    for _ in 0..7 {
        admitted.push(core.join(&token, Value::Null).await);
    }
    core.calls_become(8).await;
    for _ in 0..4 {
        let mut refused = Browser::new(core.open_call(&token).await);
        // A full room refuses right after the hello.
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
    let (mut peer, binding) = connector_with_binding(&core, "typed", THREAD).await;
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
    assert_eq!(delivery["binding_id"], binding);
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
            "session_id": session, "revision": revision, "epoch":0, "text": text, "language": "en"})
    };
    let answer = peer
        .call(
            "speech.publish",
            publish("reply", &session, revision, "Reply from the connector"),
        )
        .await;
    assert_eq!(
        (answer["status"].as_str(), answer["text_saved"].as_bool()),
        (Some("queued"), Some(true))
    );
    let reply = browser.frame("voice-reply").await;
    assert_eq!(reply["text"], "Reply from the connector");
    assert_eq!(reply["session_id"], session.as_str());
    browser.played("reply").await;
    // A report on a reply that already ended is taken and changes nothing.
    browser.playback("reply", "playing").await;
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

    // Replies go out at once, in order; the browser's voice module plays them, and the history shows what it says.
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
            .await;
        assert_eq!(answer["status"], "queued");
        assert_eq!(
            browser.frame("voice-reply").await["utterance_id"],
            utterance.as_str()
        );
    }
    browser.played("queued-2").await;
    browser
        .report(
            "voice-playback",
            json!({"utterance_id": "queued-3", "status": "interrupted"}),
        )
        .await;
    browser
        .hearing(
            &format!("{session}:voice:queued-3"),
            "queued-3",
            "heard",
            None,
        )
        .await;
    let history = core.history(&token, THREAD).await;
    let assistant_statuses: Vec<_> = history
        .iter()
        .filter(|row| row["role"] == "assistant")
        .map(|row| row["status"].clone())
        .collect();
    let statuses = &assistant_statuses[assistant_statuses.len() - 2..];
    assert_eq!(
        statuses,
        [json!("playback_finished"), json!("interrupted")],
        "{history:?}"
    );
    assert!(history
        .iter()
        .filter(|row| row["role"] == "assistant")
        .rev()
        .take(2)
        .all(|row| row["hearing"] == "heard"));

    // A browser that comes to the conversation is not played what it never heard on it.
    let mut returning = core.join(&token, Value::Null).await;
    core.select(&token, &returning.session, THREAD).await;
    returning
        .none_of(&["voice-reply"], Duration::from_millis(1500))
        .await;
    returning.close().await;

    // Two browsers on the same conversation hear the same reply; one finishing it is enough.
    let mut second = core.join(&token, Value::Null).await;
    core.select(&token, &second.session, THREAD).await;
    let answer = peer
        .call(
            "speech.publish",
            publish("both", &session, revision, "One reply for both listeners"),
        )
        .await;
    assert_eq!(answer["status"], "queued");
    assert_eq!(browser.frame("voice-reply").await["utterance_id"], "both");
    assert_eq!(second.frame("voice-reply").await["utterance_id"], "both");
    browser.playback("both", "heard").await;
    browser
        .hearing(&format!("{session}:voice:both"), "both", "heard", None)
        .await;
    second
        .report(
            "voice-playback",
            json!({"utterance_id": "both", "status": "interrupted"}),
        )
        .await;
    let history = core.history(&token, THREAD).await;
    assert_eq!(
        history
            .iter()
            .rev()
            .find(|row| row["role"] == "assistant")
            .unwrap()["status"],
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
    let (mut peer, _) = connector_with_binding(&core, "scan", THREAD).await;
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
        &ask["id"],
        json!({"agents": [], "custom": {}, "scanned_at": null}),
    );
    assert_eq!(scan.await.unwrap().status, 200);

    let listing = tokio::spawn(core.get("/api/host/agents").token(&token).send());
    let ask = peer.event("agents.list").await;
    peer.answer(
        &ask["id"],
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
async fn a_host_agent_check_reaches_the_connector_and_an_unknown_action_does_not() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (mut peer, _) = connector_with_binding(&core, "check", THREAD).await;

    let check = tokio::spawn(
        core.post("/api/host/agents/claude/check", json!({}))
            .token(&token)
            .send(),
    );
    let ask = peer.event("agents.check").await;
    assert_eq!(ask["params"], json!({"id": "claude"}));
    let agents = json!([
        {"id": "claude", "registration": "connected",
         "health": {"state": "working", "error": null, "checked_at": "2026-10-12T08:14:03.512Z"}},
        {"id": "codex", "registration": "connected",
         "health": {"state": "not-starting", "error": "command not found", "checked_at": "2026-10-12T08:14:04.001Z"}},
    ]);
    peer.answer(
        &ask["id"],
        json!({"agents": agents, "custom": {}, "scanned_at": null}),
    );
    let checked = check.await.unwrap();
    assert_eq!(checked.status, 200);
    assert_eq!(checked.json()["agents"], agents);

    // A connector that does not know the check refuses it by key, which the page receives as such.
    let older = tokio::spawn(
        core.post("/api/host/agents/claude/check", json!({}))
            .token(&token)
            .send(),
    );
    let ask = peer.event("agents.check").await;
    peer.answer(
        &ask["id"],
        json!({"error": {"key": "connector.unknown-method", "message": "unknown method", "params": {}}}),
    );
    let refused = older.await.unwrap();
    assert_eq!(refused.status, 409, "{refused:?}");
    assert_eq!(refused.json()["error"]["key"], "connector.unknown-method");

    let unknown = core
        .post("/api/host/agents/claude/explode", json!({}))
        .token(&token)
        .send()
        .await;
    assert_eq!(unknown.status, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnected_connector_keeps_its_binding_and_a_replaced_one_settles_nothing() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (peer, binding) = connector_with_binding(&core, "reconnect", THREAD).await;
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
    let (mut second, again) = connector_with_binding(&core, "reconnect", THREAD).await;
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
    let (mut third, again) = connector_with_binding(&core, "reconnect", THREAD).await;
    assert_eq!(again, binding);
    accept_delivery(&mut third).await;
    browser.receipt("delivered").await;
    second.answer(&held["id"], json!({"status": "accepted"}));
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
async fn a_paired_device_reads_the_sidevoice_version_the_linked_connector_said() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let status = || async { core.get("/api/host/status").token(&token).send().await };
    let core_version = env!("CARGO_PKG_VERSION");
    let unknown = status().await;
    assert_eq!(unknown.status, 200, "{unknown:?}");
    assert_eq!(
        unknown.json(),
        json!({"version": null, "core_version": core_version}),
        "no connector linked: unknown, not guessed"
    );

    let mut peer = Rpc::open(&core).await;
    let welcome = peer
        .request(
            "connector.hello",
            json!({"protocol": 3, "connector_id": core.connector_id(), "token": core.connector_token(),
                "host": "studio", "platform": "Linux x64", "version": " 0.8.1 "}),
        )
        .await;
    assert_eq!(welcome["result"]["protocol"], 3, "{welcome}");
    assert_eq!(
        status().await.json(),
        json!({"version": "0.8.1", "core_version": core_version})
    );

    peer.disconnect();
    eventually(STEP, "the version to be unknown again", || async {
        (status().await.json()["version"].is_null()).then_some(())
    })
    .await;

    // A reconnect that says no usable version reports none, not the one said before.
    let hello = |version: Option<Value>| {
        let mut params = json!({"protocol": 3, "connector_id": core.connector_id(),
            "token": core.connector_token(), "host": "studio", "platform": "Linux x64"});
        if let Some(version) = version {
            params["version"] = version;
        }
        params
    };
    for (case, said) in [
        ("missing", None),
        ("null", Some(Value::Null)),
        ("blank", Some(json!("  "))),
        ("not a string", Some(json!(8))),
        ("control characters only", Some(json!("\u{0}\u{1b}"))),
    ] {
        for (version, expected) in [(Some(json!("0.8.1")), json!("0.8.1")), (said, Value::Null)] {
            let mut peer = Rpc::open(&core).await;
            let welcome = peer.request("connector.hello", hello(version)).await;
            assert_eq!(welcome["result"]["protocol"], 3, "{case}: {welcome}");
            assert_eq!(status().await.json()["version"], expected, "{case}");
            peer.disconnect();
            eventually(STEP, "the connector to unlink", || async {
                (status().await.json()["version"].is_null()).then_some(())
            })
            .await;
        }
    }
    let mut peer = Rpc::open(&core).await;
    let welcome = peer
        .request("connector.hello", hello(Some(json!("\u{7}0.9.0\n"))))
        .await;
    assert_eq!(welcome["result"]["protocol"], 3, "{welcome}");
    assert_eq!(
        status().await.json()["version"],
        "0.9.0",
        "control characters are dropped"
    );
    peer.disconnect();

    assert_eq!(
        core.get("/api/host/status").send().await.status,
        401,
        "only a paired device reads it"
    );
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

    let current_revision = browser.say("A spoken update").await;
    browser.receipt("pending").await;
    let spoken = rpc.method("input.deliver", STEP).await;
    rpc.answer(&spoken["id"], json!({"status":"accepted"}));
    browser.receipt("delivered").await;

    let publish = |utterance: &str, revision: u64, epoch: u64, text: &str| {
        json!({"event_id": format!("event-{utterance}"), "utterance_id": utterance, "binding_id": binding,
            "session_id": session, "revision": revision, "epoch":epoch, "text": text})
    };
    let stale = rpc
        .request(
            "speech.publish",
            publish("stale", revision, 0, "A reply to an older spoken epoch"),
        )
        .await;
    assert_eq!(stale["result"]["status"], "text_only", "{stale}");
    assert!(stale["result"].get("reason").is_none(), "{stale}");
    let reply = rpc
        .request(
            "speech.publish",
            publish("current", current_revision, 1, "Reply over JSON-RPC"),
        )
        .await;
    assert_eq!(reply["result"]["status"], "queued", "{reply}");
    assert_eq!(reply["result"]["text_saved"], true, "{reply}");
    assert_eq!(
        browser.frame("voice-reply").await["text"],
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

/// The protocol only grows (#112): a delivery answer from a newer connector may carry keys this core does not read
/// (sidevoice-connector#115 added `reason`), or a status it does not know. The first still settles the delivery; the
/// second is tried again, as a failed one is.
#[tokio::test(flavor = "multi_thread")]
async fn a_delivery_answer_with_keys_the_core_does_not_read_still_settles_it() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (mut peer, _) = connector_with_binding(&core, "answers", THREAD).await;
    let focus = core.select(&token, &session, THREAD).await;

    let sent = core
        .text(&token, (&session, THREAD, &focus), "First", &message_id())
        .await;
    let delivery = peer.event("input.deliver").await;
    peer.answer(
        &delivery["id"],
        json!({"status": "accepted", "reason": "queued_in_harness", "error": {"nested": true},
            "added_later": {"nested": {"deeper": [1, 2]}}}),
    );
    assert_eq!(browser.receipt("delivered").await["history_id"], sent["id"]);

    let sent = core
        .text(&token, (&session, THREAD, &focus), "Second", &message_id())
        .await;
    let delivery = peer.event("input.deliver").await;
    peer.answer(&delivery["id"], json!({"status": "deferred_by_harness"}));
    let again = peer.event("input.deliver").await;
    assert_eq!(again["params"]["event_id"], delivery["params"]["event_id"]);
    peer.answer(&again["id"], json!({"status": "accepted"}));
    assert_eq!(browser.receipt("delivered").await["history_id"], sent["id"]);
}

/// A registration from a newer connector, with capabilities, a route and fields this core does not
/// know: it binds, and the capabilities it does not know are left out.
#[tokio::test(flavor = "multi_thread")]
async fn a_registration_with_values_the_core_does_not_know_binds_as_push() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let mut peer = Rpc::hello(&core).await;
    let mut registration = registration("future", THREAD);
    registration["route"] = json!("future-route");
    registration["capabilities"] =
        json!({"deliver": "supported", "working": "partially", "teleport": "supported"});
    registration["engine"] = json!({"model": "m", "context": {"window": 1}});
    registration["added_later"] = json!({"nested": {"deeper": true}});
    let bound = peer.call("binding.register", registration).await;
    assert_eq!(bound["thread"], THREAD, "{bound}");
    let binding = bound["binding_id"].as_str().unwrap().to_owned();
    let listing = core
        .get("/api/connectors")
        .token(&token)
        .send()
        .await
        .json();
    let view = listing["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["id"] == binding.as_str())
        .cloned()
        .expect("the binding is listed");
    assert_eq!(view["capabilities"]["deliver"], "supported");
    assert_eq!(view["capabilities"]["working"], "unknown");
    assert!(view["capabilities"].get("teleport").is_none(), "{view}");
    assert!(view.get("input_mode").is_none());
    assert_eq!(view["route"], Value::Null);

    let focus = core.select(&token, &session, THREAD).await;
    core.text(&token, (&session, THREAD, &focus), "Pushed", &message_id())
        .await;
    assert_eq!(
        accept_delivery(&mut peer).await["binding_id"],
        binding.as_str()
    );
    browser.receipt("delivered").await;
}

/// A reply in a language this core does not speak is kept and sent to the call as one that names no language.
#[tokio::test(flavor = "multi_thread")]
async fn a_reply_in_a_language_the_core_does_not_know_is_still_sent() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (mut peer, binding) = connector_with_binding(&core, "language", THREAD).await;
    core.select(&token, &session, THREAD).await;
    let revision = core.revision(&token, &session).await;
    let answer = peer
        .call(
            "speech.publish",
            json!({"event_id": "event-de", "utterance_id": "de", "binding_id": binding,
                "session_id": session, "revision": revision, "epoch":0, "text": "Guten Tag.", "language": "de"}),
        )
        .await;
    assert_eq!(answer["status"], "queued", "{answer}");
    let reply = browser.frame("voice-reply").await;
    assert_eq!(
        (&reply["text"], &reply["language"]),
        (&json!("Guten Tag."), &Value::Null)
    );
}

/// Every message a connector sends, with fields this core does not know at the top level, nested and next to `jsonrpc`
/// and `method`: the hello, a registration, its reports, a delivery answer, a reply, a pull, answers about host agents
/// and an unregistration. All are taken as their known part says.
#[tokio::test(flavor = "multi_thread")]
async fn every_connector_message_with_fields_the_core_does_not_know_is_taken() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let extra = json!({"nested": {"deeper": [1, {"x": true}]}});

    let mut peer = Rpc::open(&core).await;
    peer.send(json!({"jsonrpc": "2.0", "id": 1, "method": "connector.hello", "added_later": extra,
        "params": {"protocol": 3, "connector_id": core.connector_id(), "token": core.connector_token(),
            "version": "9.9.9", "added_later": extra}}));
    let welcome = peer.frame(STEP, |frame| frame["id"] == 1).await;
    assert_eq!(welcome["result"]["protocol"], 3, "{welcome}");

    let mut registration = registration("sweep", THREAD);
    registration["added_later"] = extra.clone();
    let binding = peer.call("binding.register", registration).await["binding_id"]
        .as_str()
        .expect("a binding")
        .to_owned();
    let focus = core.select(&token, &session, THREAD).await;

    // An extra key next to `jsonrpc` and `method`.
    peer.send(
        json!({"jsonrpc": "2.0", "method": "input.working", "added_later": extra,
        "params": {"binding_id": binding, "working": true, "added_later": extra}}),
    );
    assert_eq!(browser.frame("voice-conversation").await["working"], true);
    peer.emit(
        "input.engine",
        json!({"binding_id": binding, "engine": {"model": "m2", "added_later": extra}, "added_later": extra}),
    );
    eventually(STEP, "the binding to show the engine it changed to", || {
        let listing = core.get("/api/connectors").token(&token).send();
        let binding = binding.clone();
        async move {
            let listing = listing.await.json();
            let view = listing["bindings"]
                .as_array()?
                .iter()
                .find(|view| view["id"] == binding.as_str())?
                .clone();
            (view["engine"] == json!({"model": "m2"})).then_some(())
        }
    })
    .await;

    let message = message_id();
    let sent = core
        .text(&token, (&session, THREAD, &focus), "Swept", &message)
        .await;
    let delivery = peer.event("input.deliver").await;
    peer.answer(
        &delivery["id"],
        json!({"status": "accepted", "detail": "taken", "added_later": extra}),
    );
    assert_eq!(browser.receipt("delivered").await["history_id"], sent["id"]);
    peer.emit(
        "input.read",
        json!({"binding_id": binding, "message_id": message, "added_later": extra}),
    );
    assert_eq!(browser.receipt("read").await["history_id"], sent["id"]);

    let revision = core.revision(&token, &session).await;
    let published = peer
        .call(
            "speech.publish",
            json!({"event_id": "event-swept", "utterance_id": "swept", "binding_id": binding,
                "session_id": session, "revision": revision, "epoch":0, "text": "Swept reply.", "language": "en",
                "added_later": extra}),
        )
        .await;
    assert_eq!(published["status"], "queued", "{published}");
    assert_eq!(browser.frame("voice-reply").await["text"], "Swept reply.");

    let pulled = peer
        .call(
            "binding.register",
            json!({"thread": "pull-thread", "harness": "fixture", "capabilities":{"deliver":"unsupported"}, "added_later": extra}),
        )
        .await;
    let checked = peer
        .call(
            "input.pull",
            json!({"binding_id": pulled["binding_id"], "operation": "check", "added_later": extra}),
        )
        .await;
    assert_eq!(checked["count"], 0, "{checked}");

    // A query parameter the core does not read, and an answer with keys it does not read.
    let listing = tokio::spawn(
        core.get("/api/host/agents?added_later=1")
            .token(&token)
            .send(),
    );
    let ask = peer.event("agents.list").await;
    assert_eq!(ask["params"], json!({"rescan": false}));
    let agents = json!([{"id": "claude", "registration": "connected", "added_later": extra}]);
    peer.answer(
        &ask["id"],
        json!({"agents": agents, "custom": {"added_later": extra}, "scanned_at": null, "added_later": extra}),
    );
    let listed = listing.await.unwrap();
    assert_eq!(listed.status, 200, "{listed:?}");
    assert_eq!(listed.json()["agents"], agents);

    peer.emit(
        "binding.unregister",
        json!({"binding_id": binding, "added_later": extra}),
    );
    eventually(
        STEP,
        "the unregistered binding to leave the listing",
        || {
            let listing = core.get("/api/connectors").token(&token).send();
            let binding = binding.clone();
            async move {
                let listing = listing.await.json();
                (!listing["bindings"]
                    .as_array()?
                    .iter()
                    .any(|view| view["id"] == binding.as_str()))
                .then_some(())
            }
        },
    )
    .await;
}
