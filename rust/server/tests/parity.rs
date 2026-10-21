//! Contracts the clients rely on: open routes, pairing codes, origin refusals and the
//! call socket's admission.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as Frame};
use tower::ServiceExt;

use super::support::{app_state, private_dir};
use crate::control::room::Room;
use crate::server::call::Reattach;
use crate::server::guard::open_route;
use crate::server::{router, AppState};

/// A node with one paired device, and that device's token.
fn paired() -> (tempfile::TempDir, Arc<AppState>, String) {
    let (temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let state = app_state(&dir, room, "host");
    let (_, token, _) = state
        .registry
        .lock()
        .unwrap()
        .pair_local(Some("Test device"))
        .unwrap();
    (temp, state, token)
}

async fn body_json(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
}

#[test]
fn only_discovery_and_device_pairing_need_no_token() {
    assert!(open_route(&Method::GET, "/api/rendezvous", false));
    assert!(!open_route(&Method::POST, "/api/rendezvous", false));
    assert!(open_route(&Method::POST, "/api/device/pair", false));
    assert!(!open_route(&Method::GET, "/api/local/health", false));
    assert!(open_route(&Method::GET, "/api/local/health", true));
}

#[tokio::test]
async fn foreign_origins_are_refused_by_host_agents() {
    let (_temp, state, token) = paired();
    let app = router(state, false);
    let request = |method: &str, uri: &str, body: Body| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost:10829")
            .header("origin", "https://elsewhere.example")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    };
    let agents = app
        .clone()
        .oneshot(request("GET", "/api/host/agents", Body::empty()))
        .await
        .unwrap();
    assert_eq!(agents.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(agents).await, json!({"key":"origin-not-allowed"}));
    let status = app
        .clone()
        .oneshot(request("GET", "/api/host/status", Body::empty()))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(status).await, json!({"key":"origin-not-allowed"}));
    let action = app
        .oneshot(request(
            "POST",
            "/api/host/agents/codex/connect",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(action.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(action).await, json!({"key":"origin-not-allowed"}));
}

type CallSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn call_socket_client(state: Arc<AppState>, token: &str) -> CallSocket {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router(state, false)).await.unwrap();
    });
    let mut request = format!("ws://127.0.0.1:{port}/api/presentation/ws")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(&format!("sidevoice, sidevoice.token.{token}")).unwrap(),
    );
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

/// Takes every seat in the room for someone else; the seats are held while the receivers live.
fn fill_room(state: &AppState) -> Vec<tokio::sync::mpsc::Receiver<Value>> {
    let mut held = Vec::new();
    loop {
        let (events, receiver) = tokio::sync::mpsc::channel(8);
        if state
            .room
            .join("other".into(), "en".into(), events)
            .is_err()
        {
            return held;
        }
        held.push(receiver);
    }
}

async fn hello(socket: &mut CallSocket, data: Value) {
    socket
        .send(Frame::text(
            json!({"type":"client-ready","data":data}).to_string(),
        ))
        .await
        .unwrap();
}

async fn next_session(socket: &mut CallSocket) -> Value {
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("session frame arrives")
            .unwrap()
            .unwrap();
        if let Frame::Text(text) = frame {
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["type"] == "voice-session" {
                return value;
            }
        }
    }
}

#[tokio::test]
async fn resumed_hello_applies_output_off_before_new_publication() {
    let (_temp, state, token) = paired();
    let mut first = call_socket_client(state.clone(), &token).await;
    hello(&mut first, json!({"speech":{"on":true}})).await;
    let session = next_session(&mut first).await;
    let sid = session["data"]["session_id"].as_str().unwrap().to_owned();
    let resume_token = session["data"]["resume"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let seq = session["seq"].as_u64().unwrap_or(0);
    state.room.select(&sid, "t").unwrap();
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut second = call_socket_client(state.clone(), &token).await;
    hello(
        &mut second,
        json!({"resume":{"session_id":sid,"token":resume_token,"last_seq":seq},
        "speech":{"on":false,"reason":"voice_off"}}),
    )
    .await;
    assert_eq!(next_session(&mut second).await["data"]["resumed"], true);
    let revision = state.room.snapshot(Some(&sid))["room"]["revision"]
        .as_u64()
        .unwrap();
    let result = state.room.publish(
        &json!({"session_id":sid,"thread_id":"t","revision":revision,
        "epoch":0,"utterance_id":"off-after-resume","text":"Do not play"}),
    );
    assert_eq!(result["status"], "text_only");
    assert!(result.get("reason").is_none());
}

#[tokio::test]
async fn resumed_hello_applies_output_on_before_new_publication() {
    let (_temp, state, token) = paired();
    let mut first = call_socket_client(state.clone(), &token).await;
    hello(
        &mut first,
        json!({"speech":{"on":false,"reason":"voice_off"}}),
    )
    .await;
    let session = next_session(&mut first).await;
    let sid = session["data"]["session_id"].as_str().unwrap().to_owned();
    let resume_token = session["data"]["resume"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let seq = session["seq"].as_u64().unwrap_or(0);
    state.room.select(&sid, "t").unwrap();
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut second = call_socket_client(state.clone(), &token).await;
    hello(
        &mut second,
        json!({"resume":{"session_id":sid,"token":resume_token,"last_seq":seq},
        "speech":{"on":true}}),
    )
    .await;
    assert_eq!(next_session(&mut second).await["data"]["resumed"], true);
    let revision = state.room.snapshot(Some(&sid))["room"]["revision"]
        .as_u64()
        .unwrap();
    let result = state.room.publish(
        &json!({"session_id":sid,"thread_id":"t","revision":revision,
        "epoch":0,"utterance_id":"on-after-resume","text":"Play now"}),
    );
    assert_eq!(result["status"], "queued");
}

#[tokio::test]
async fn a_full_room_is_said_right_after_the_hello() {
    let (_temp, state, token) = paired();
    let _held = fill_room(&state);
    let mut socket = call_socket_client(state, &token).await;
    // A resume naming no call this device has is a new call, and a new call finds the room full.
    hello(
        &mut socket,
        json!({"resume":{"session_id":"gone","token":"t","last_seq":3}}),
    )
    .await;
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("the refusal follows the hello")
        .unwrap()
        .unwrap();
    let Frame::Text(text) = first else {
        panic!("expected the refusal frame, got {first:?}");
    };
    let refusal: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(refusal["type"], "error");
    assert_eq!(refusal["data"]["reason"], "room_is_full");
    let Some(Ok(Frame::Close(Some(close)))) = socket.next().await else {
        panic!("expected a close frame");
    };
    assert_eq!(u16::from(close.code), 1013);
}

#[tokio::test]
async fn a_returning_page_takes_its_call_back_once_even_in_a_full_room() {
    let (_temp, state, token) = paired();
    let device = state.authenticate_token(&token).unwrap();
    let _held = fill_room(&state);
    let (attach, mut parked) = tokio::sync::mpsc::channel::<Reattach>(1);
    state
        .resumable
        .open("parked-session", &device, "secret", attach);
    let mut socket = call_socket_client(state.clone(), &token).await;
    hello(
        &mut socket,
        json!({"resume":{"session_id":"parked-session","token":"secret","last_seq":7}}),
    )
    .await;
    let reattach = tokio::time::timeout(std::time::Duration::from_secs(5), parked.recv())
        .await
        .expect("the call is handed the new socket")
        .unwrap();
    assert_eq!(reattach.last_seq, 7);
    let _ = reattach.answer.send(Ok(()));
    // The token was spent: the same one, again, is a new call that finds the room full.
    let mut again = call_socket_client(state, &token).await;
    hello(
        &mut again,
        json!({"resume":{"session_id":"parked-session","token":"secret","last_seq":7}}),
    )
    .await;
    let Some(Ok(Frame::Text(text))) = again.next().await else {
        panic!("expected the refusal frame");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["data"]["reason"],
        "room_is_full"
    );
    assert!(parked.try_recv().is_err(), "a spent token reaches no call");
}

#[tokio::test]
async fn another_device_cannot_take_a_call_over() {
    let (_temp, state, token) = paired();
    let _held = fill_room(&state);
    let (attach, mut parked) = tokio::sync::mpsc::channel::<Reattach>(1);
    state
        .resumable
        .open("parked-session", "someone-else", "secret", attach);
    let mut socket = call_socket_client(state, &token).await;
    hello(
        &mut socket,
        json!({"resume":{"session_id":"parked-session","token":"secret","last_seq":0}}),
    )
    .await;
    let Some(Ok(Frame::Text(text))) = socket.next().await else {
        panic!("expected the refusal frame");
    };
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["data"]["reason"],
        "room_is_full"
    );
    assert!(parked.try_recv().is_err());
}

#[tokio::test]
async fn no_hello_in_time_goes_on_with_the_default_settings() {
    let (_temp, state, token) = paired();
    let mut socket = call_socket_client(state, &token).await;
    let first = tokio::time::timeout(std::time::Duration::from_secs(20), socket.next())
        .await
        .expect("the call answers after the hello timeout")
        .expect("the socket stays open past the hello timeout")
        .unwrap();
    // Whatever the defaults lead to on this machine (a session, or a reason it cannot start one), the call speaks
    // instead of closing in silence.
    assert!(matches!(first, Frame::Text(_)), "{first:?}");
}
