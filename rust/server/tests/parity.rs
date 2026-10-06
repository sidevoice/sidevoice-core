//! Contracts the clients rely on: open routes, pairing codes, origin refusals and the
//! call socket's admission.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as Frame};
use tower::ServiceExt;

use super::support::{app_state, private_dir};
use crate::control::room::Room;
use crate::server::guard::open_route;
use crate::server::node::pair_refused;
use crate::server::{advertisable_room, router, AppState};

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
fn the_dial_link_needs_no_token_for_any_method() {
    for method in [Method::GET, Method::POST] {
        for path in [
            "/api/rendezvous/link",
            "/api/rendezvous/link/",
            "/api/rendezvous/link/x",
        ] {
            assert!(open_route(&method, path, false), "{method} {path}");
        }
    }
    assert!(!open_route(&Method::POST, "/api/rendezvous/linked", false));
    assert!(!open_route(&Method::POST, "/api/rendezvous", false));
    assert!(open_route(&Method::POST, "/api/device/pair", false));
    assert!(!open_route(&Method::GET, "/api/local/health", false));
    assert!(open_route(&Method::GET, "/api/local/health", true));
}

#[test]
fn a_pairing_code_names_the_room_only_over_a_safe_transport() {
    let room = |url: &str| Some(json!({"url": url, "node": "connector"}));
    assert_eq!(
        advertisable_room(room("https://room.example")),
        room("https://room.example")
    );
    assert_eq!(advertisable_room(room("http://room.example")), None);
    assert_eq!(advertisable_room(None), None);
}

#[tokio::test]
async fn a_refused_pairing_keeps_the_connectors_reason() {
    let headers = HeaderMap::new();
    let refused = pair_refused(
        &json!({"ok":false,"detail":"That code has expired."}),
        &headers,
    );
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(refused).await,
        json!({"detail":"That code has expired."})
    );
    let generic = pair_refused(&json!({"ok":false}), &headers);
    assert_eq!(generic.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(generic).await,
        json!({"detail":"The room pairing failed."})
    );
}

#[tokio::test]
async fn foreign_origins_are_refused_by_host_agents_and_the_rtc_offer() {
    let (_temp, state, token) = paired();
    let app = router(state, false);
    let request = |method: &str, uri: &str, body: Body| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "localhost:8768")
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
    let action = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/host/agents/codex/connect",
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(action.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(action).await, json!({"key":"origin-not-allowed"}));
    let offer = serde_json::to_vec(&json!({"type":"offer","session_id":"s","sdp":"v=0"})).unwrap();
    let rtc = app
        .oneshot(request(
            "POST",
            "/api/presentation/rtc/offer",
            Body::from(offer),
        ))
        .await
        .unwrap();
    assert_eq!(rtc.status(), StatusCode::FORBIDDEN);
}

async fn call_socket_client(
    state: Arc<AppState>,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
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

#[tokio::test]
async fn a_full_room_is_said_before_the_hello() {
    let (_temp, state, token) = paired();
    let mut held = Vec::new();
    loop {
        let (events, receiver) = tokio::sync::mpsc::channel(8);
        if state
            .room
            .join("other".into(), "en".into(), events)
            .is_err()
        {
            break;
        }
        held.push(receiver);
    }
    let mut socket = call_socket_client(state, &token).await;
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("the refusal arrives without a hello")
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
