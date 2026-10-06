use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::support::{app_state, private_dir};
use crate::control::room::Room;
use crate::server::router;

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
