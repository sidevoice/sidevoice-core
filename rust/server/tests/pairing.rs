use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::support::{app_state, private_dir};
use crate::control::room::Room;
use crate::server::limits::{Limits, AUTH_PER_PEER, PAIR_GLOBAL, PAIR_PER_PEER};
use crate::server::router;

fn request(method: &str, path: &str, body: Body, peer: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .body(body)
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
    request
}

#[tokio::test]
async fn pairing_failures_back_off_per_peer_then_recover() {
    let (_temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let mut state = app_state(&dir, room, "fixture-host");
    Arc::get_mut(&mut state).unwrap().limits = Limits::short_window();
    let secret = state.issue_code()["payload"]["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = state.issue_code()["payload"]["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let app = router(state, false);
    for _ in 0..PAIR_PER_PEER {
        let answer = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/device/pair",
                Body::from(r#"{"secret":"wrong"}"#),
                "192.0.2.1:1000",
            ))
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::FORBIDDEN);
    }
    let answer = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/device/pair",
            Body::from(r#"{"secret":"wrong"}"#),
            "192.0.2.1:1001",
        ))
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(answer.headers()["retry-after"], "1");
    // A different TCP peer has its own budget.
    let valid = serde_json::to_vec(&json!({"secret": secret})).unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(
                "POST",
                "/api/device/pair",
                Body::from(valid.clone()),
                "192.0.2.2:1000"
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let valid = serde_json::to_vec(&json!({"secret": second})).unwrap();
    assert_eq!(
        app.oneshot(request(
            "POST",
            "/api/device/pair",
            Body::from(valid),
            "192.0.2.1:1000"
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn pairing_global_budget_covers_rotating_peers() {
    let (_temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let state = app_state(&dir, room, "fixture-host");
    let app = router(state, false);
    for number in 0..PAIR_GLOBAL {
        let peer = format!("192.0.2.{}:1000", number + 1);
        let answer = app
            .clone()
            .oneshot(request(
                "POST",
                "/api/device/pair",
                Body::from(r#"{"secret":"wrong"}"#),
                &peer,
            ))
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::FORBIDDEN);
    }
    let answer = app
        .oneshot(request(
            "POST",
            "/api/device/pair",
            Body::from(r#"{"secret":"wrong"}"#),
            "198.51.100.1:1000",
        ))
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn failed_auth_is_limited_but_valid_devices_and_unix_are_not() {
    let (_temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let state = app_state(&dir, room, "fixture-host");
    let secret = state.issue_code()["payload"]["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let tcp = router(state.clone(), false);
    let unix = router(state, true);
    let valid = serde_json::to_vec(&json!({"secret": secret})).unwrap();
    let paired = tcp
        .clone()
        .oneshot(request(
            "POST",
            "/api/device/pair",
            Body::from(valid),
            "192.0.2.1:1000",
        ))
        .await
        .unwrap();
    let payload: Value =
        serde_json::from_slice(&to_bytes(paired.into_body(), 4096).await.unwrap()).unwrap();
    let token = payload["token"].as_str().unwrap();
    for _ in 0..AUTH_PER_PEER {
        let answer = tcp
            .clone()
            .oneshot(request(
                "GET",
                "/api/device/devices",
                Body::empty(),
                "192.0.2.1:1001",
            ))
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::UNAUTHORIZED);
    }
    let answer = tcp
        .clone()
        .oneshot(request(
            "GET",
            "/api/device/devices",
            Body::empty(),
            "192.0.2.1:1002",
        ))
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(answer.headers().contains_key("retry-after"));
    let mut good = request(
        "GET",
        "/api/device/devices",
        Body::empty(),
        "192.0.2.1:1003",
    );
    good.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    assert_eq!(tcp.oneshot(good).await.unwrap().status(), StatusCode::OK);
    for _ in 0..AUTH_PER_PEER + 2 {
        let answer = unix
            .clone()
            .oneshot(request(
                "GET",
                "/api/device/devices",
                Body::empty(),
                "192.0.2.1:1004",
            ))
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        unix.oneshot(request(
            "GET",
            "/api/local/health",
            Body::empty(),
            "192.0.2.1:1005"
        ))
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn refused_origins_use_the_failed_auth_budget() {
    let (_temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let state = app_state(&dir, room, "fixture-host");
    let app = router(state, false);
    for _ in 0..AUTH_PER_PEER {
        let mut attempt = request(
            "GET",
            "/api/device/identity?nonce=bad",
            Body::empty(),
            "192.0.2.1:1000",
        );
        attempt
            .headers_mut()
            .insert("origin", "https://attacker.example".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(attempt).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let mut attempt = request(
        "GET",
        "/api/device/identity?nonce=bad",
        Body::empty(),
        "192.0.2.1:1000",
    );
    attempt
        .headers_mut()
        .insert("origin", "https://attacker.example".parse().unwrap());
    assert_eq!(
        app.oneshot(attempt).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn one_time_code_redeems_over_real_router() {
    let (_temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let state = app_state(&dir, room, "fixture-host");
    let code = state.issue_code();
    assert!(code["code"].as_str().unwrap().starts_with("SV1."));
    let secret = code["payload"]["secret"].as_str().unwrap();
    let body = serde_json::to_vec(&json!({"secret": secret, "name": "Test device"})).unwrap();
    let app = router(state, false);
    let pair = || {
        Request::builder()
            .method("POST")
            .uri("/api/device/pair")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(body.clone()))
            .unwrap()
    };
    let first = app.clone().oneshot(pair()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let response: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), 4096).await.unwrap()).unwrap();
    let token = response["token"].as_str().unwrap();
    let list = Request::builder()
        .uri("/api/device/devices")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(list).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        app.oneshot(pair()).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        dir.read_json("devices.json").unwrap().unwrap()["devices"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
}

/// The protocol only grows (#112): a pairing body with fields this core does not know still redeems its code.
#[tokio::test]
async fn a_pairing_body_with_fields_the_core_does_not_know_still_redeems() {
    let (_temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let state = app_state(&dir, room, "fixture-host");
    let code = state.issue_code();
    let secret = code["payload"]["secret"].as_str().unwrap();
    let body =
        json!({"secret": secret, "name": "Test device", "added_later": {"nested": [1, true]}});
    let pair = Request::builder()
        .method("POST")
        .uri("/api/device/pair")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let answer = router(state, false).oneshot(pair).await.unwrap();
    assert_eq!(answer.status(), StatusCode::OK);
    let answer: Value =
        serde_json::from_slice(&to_bytes(answer.into_body(), 4096).await.unwrap()).unwrap();
    assert!(answer["token"].is_string(), "{answer}");
}
