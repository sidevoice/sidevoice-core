//! The core's link to its paired room, from the room's side: the core dials out to the room's Socket.IO
//! namespace, or the room dials in to the core's; either way the room relays HTTP requests and call sockets
//! through it, with binary bodies as Socket.IO attachments. The link follows the connector-owned pairing file:
//! a refusal stops it, a new pairing resumes it, an edited one replaces it, a removed one ends it.

mod support;

use std::path::Path;
use std::time::Duration;

use futures_util::future::join_all;
use serde_json::{json, Value};
use support::*;

fn pair(file: &Path, room: &Room, token: &str) {
    let pairing = json!({"url": format!("http://127.0.0.1:{}", room.port), "connector_id": "node-1",
        "token": token, "dial_key": "fixture-dial-key", "protocol": 3});
    std::fs::write(file, pairing.to_string()).unwrap();
}

/// The core's outbound link, accepted and welcomed by the room.
async fn linked(room: &mut Room, token: &str) -> Sio {
    let (link, auth, path) = room.accept().await;
    assert!(path.starts_with("/api/connectors/link"), "{path}");
    assert_eq!(
        (
            auth["connector_id"].as_str(),
            auth["token"].as_str(),
            auth["protocol"].as_u64()
        ),
        (Some("node-1"), Some(token), Some(3)),
        "{auth}"
    );
    assert!(
        auth["core"]
            .as_str()
            .is_some_and(|version| !version.is_empty()),
        "{auth}"
    );
    link.emit(
        "node.welcome",
        json!({"protocol": 3, "public_url": "https://room.example"}),
    );
    link
}

/// The admission check a page makes first, relayed: the body comes back as a binary attachment.
async fn admitted(link: &Sio, token: &str) {
    let answer = link
        .call_within(
            "relay.http",
            json!({"method": "GET", "path": "/api/presentation/admission",
                "headers": {"accept": "application/json", "authorization": format!("Bearer {token}")}}),
            Vec::new(),
            STEP,
        )
        .await
        .expect("an answer to the relayed request");
    assert_eq!(answer.data["status"], 200, "{:?}", answer.data);
    let body: Value = serde_json::from_slice(&answer.binary(&answer.data["body"])).unwrap();
    assert_eq!(body["admitted"], true, "{body}");
}

/// The call event of `kind` the core sent back on a relayed call socket.
async fn call_event(link: &mut Sio, channel: &str, kind: &str, within: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = link
            .try_event("relay.data", left)
            .await
            .unwrap_or_else(|| panic!("no {kind} on the relayed call"));
        if event.data["channel"] != channel {
            continue;
        }
        let Some(text) = event.data["data"].as_str() else {
            continue;
        };
        let frame: Value = serde_json::from_str(text).unwrap();
        if frame["type"] == kind {
            return frame["data"].clone();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_room_relays_requests_and_calls_and_the_link_follows_the_pairing() {
    let root = tempfile::tempdir().unwrap();
    let mut room = Room::listen("/nodes").await;
    let pairing = root.path().join("room-credentials.json");
    pair(&pairing, &room, "fixture-room-token");
    let core = Launch::new(root.path().join("core"))
        .arg("--room-credential")
        .arg(pairing.display().to_string())
        .start();
    let mut link = linked(&mut room, "fixture-room-token").await;
    let token = core.pair_local("Relayed browser").await;

    join_all((0..16).map(|_| admitted(&link, &token))).await;
    for path in [
        "/api/connectors/link",
        "/api/rendezvous",
        "/api/device/local/pair",
        "/api/device/%2e%2e/connectors/link",
        "/api/models/../rendezvous",
        "/api/presentation/%252e%252e/connectors",
    ] {
        let answer = link
            .call("relay.http", json!({"method": "GET", "path": path}))
            .await;
        assert_eq!(
            answer.data["status"], 404,
            "{path} is not relayed: {:?}",
            answer.data
        );
    }
    let unpaired = link
        .call(
            "relay.http",
            json!({"method": "GET", "path": "/api/host/agents"}),
        )
        .await;
    assert_eq!(unpaired.data["status"], 401);

    // A binary request body reaches the core; an attachment beside it is ignored.
    let report = json!({"kind": "fixture", "message": "binary body reached the core"});
    let recorded = link
        .call_within(
            "relay.http",
            json!({"method": "POST", "path": "/api/presentation/client-error",
                "headers": {"authorization": format!("Bearer {token}"), "content-type": "application/json"},
                "body": {"_placeholder": true, "num": 0},
                "ignored": {"nested": [{"_placeholder": true, "num": 1}]}}),
            vec![report.to_string().into_bytes(), b"second-attachment".to_vec()],
            STEP,
        )
        .await
        .expect("an answer");
    assert_eq!(recorded.data["status"], 200, "{:?}", recorded.data);
    let body: Value = serde_json::from_slice(&recorded.binary(&recorded.data["body"])).unwrap();
    assert_eq!(body["status"], "recorded");
    let snapshot = link
        .call(
            "relay.http",
            json!({"method": "GET", "path": "/api/presentation",
                "headers": {"authorization": format!("Bearer {token}")}}),
        )
        .await;
    assert_eq!(snapshot.data["status"], 200);
    let snapshot: Value = serde_json::from_slice(&snapshot.binary(&snapshot.data["body"])).unwrap();
    let errors = snapshot["room"]["client_errors"].as_array().unwrap();
    assert_eq!(errors.last().unwrap()["message"], report["message"]);

    // A whole call through the relay: hello, and a turn the call's voice module transcribed.
    let opened = link
        .call(
            "relay.open",
            json!({"channel": "call", "path": "/api/presentation/ws",
                "protocols": ["sidevoice", format!("sidevoice.token.{token}")]}),
        )
        .await;
    assert_eq!(opened.data, json!({"ok": true}));
    let hello = json!({"type": "voice-hello", "data": {"ui_language": "en"}});
    link.emit(
        "relay.data",
        json!({"channel": "call", "data": hello.to_string()}),
    );
    let session = call_event(&mut link, "call", "voice-session", STEP).await;
    let sid = session["session_id"].clone();
    let started = json!({"type": "voice-user-turn", "data": {"session_id": sid, "client_msg_id": "turn-started",
        "phase": "started"}});
    link.emit(
        "relay.data",
        json!({"channel": "call", "data": started.to_string()}),
    );
    let turn = call_event(&mut link, "call", "voice-user-turn", STEP).await;
    assert_eq!(turn["phase"], "started");
    let finished = json!({"type": "voice-user-turn", "data": {"session_id": sid, "client_msg_id": "turn-finished",
        "phase": "finished", "revision": turn["revision"], "text": "Said through the room"}});
    link.emit(
        "relay.data",
        json!({"channel": "call", "data": finished.to_string()}),
    );
    // The call is on no conversation: the words are kept as not sent.
    let receipt = call_event(&mut link, "call", "voice-input-receipt", STEP).await;
    assert_eq!(receipt["status"], "not_sent");
    assert_eq!(receipt["revision"], turn["revision"]);
    link.emit("relay.close", json!({"channel": "call", "code": 1000}));
    let denied = link
        .call(
            "relay.open",
            json!({"channel": "not-a-call", "path": "/api/connectors/v3"}),
        )
        .await;
    assert_eq!(denied.data["ok"], false, "{:?}", denied.data);

    // The room dialling in: only with the pairing's dial key, and only once it answers the core's hello.
    let refused = dial(
        &core,
        json!({"connector_id": "node-1", "dial_key": "wrong"}),
    )
    .await;
    assert!(refused.is_err(), "a wrong dial key is refused");
    let auth = json!({"connector_id": "node-1", "dial_key": "fixture-dial-key"});
    let mut dialled = dial(&core, auth.clone())
        .await
        .expect("the dial key is accepted");
    let hello = dialled.event("node.hello").await;
    assert_eq!(
        (
            hello.data["connector_id"].as_str(),
            hello.data["token"].as_str(),
            hello.data["protocol"].as_u64()
        ),
        (Some("node-1"), Some("fixture-room-token"), Some(3))
    );
    dialled.answer(
        hello.id.expect("the hello asks for an answer"),
        json!({"protocol": 3}),
    );
    admitted(&dialled, &token).await;
    dialled.disconnect();

    // The room drops the link: the core dials again.
    link.disconnect();
    let link = linked(&mut room, "fixture-room-token").await;
    admitted(&link, &token).await;

    // A refusal is final for this pairing; pairing again resumes the link.
    link.emit("node.revoked", json!({"reason": "fixture-refused"}));
    assert!(
        room.accept_within(Duration::from_secs(3)).await.is_none(),
        "a revoked pairing is not dialled again"
    );
    pair(&pairing, &room, "rotated-room-token");
    let link = linked(&mut room, "rotated-room-token").await;
    admitted(&link, &token).await;

    // The connector edits the pairing: the healthy link is replaced by one with the new credential.
    pair(&pairing, &room, "live-rotated-token");
    assert!(
        link.gone_within(Duration::from_secs(20)).await,
        "the old link ends"
    );
    let link = linked(&mut room, "live-rotated-token").await;
    admitted(&link, &token).await;

    // A dial whose hello answer is malformed ends, and the room is told why: until the pairing changes, the
    // node counts as refused.
    let mut malformed = dial(&core, auth).await.expect("the dial key is accepted");
    let hello = malformed.event("node.hello").await;
    malformed.answer(hello.id.unwrap(), json!("malformed"));
    assert!(
        malformed.gone_within(STEP).await,
        "a malformed hello ends the dial"
    );

    // The connector removes the pairing: the link ends and is not dialled again.
    std::fs::remove_file(&pairing).unwrap();
    assert!(
        link.gone_within(Duration::from_secs(20)).await,
        "the link ends with its pairing"
    );
    assert!(
        room.accept_within(Duration::from_secs(3)).await.is_none(),
        "a removed pairing is not dialled again"
    );
}
