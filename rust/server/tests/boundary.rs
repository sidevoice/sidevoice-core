//! The node's trust boundary over both listeners: what only the socket serves, and that none of it
//! exists over TCP or to a page (the relay half is beside `relayable`); the token every route but a
//! few requires, the call socket's token as a subprotocol, revocation; and CORS and preflights for
//! desktop origins, a foreign Host refused.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use tower::ServiceExt;

use super::support::{app_state, private_dir};
use crate::control::room::Room;
use crate::server::{router, AppState};
use crate::storage::PrivateDir;

const APP: &str = "tauri://localhost";
const LOCAL_PATHS: [(&str, &str); 4] = [
    ("GET", "/api/local/health"),
    ("POST", "/api/device/local/pair"),
    ("DELETE", "/api/device/local"),
    ("GET", "/api/connectors/link/?EIO=4&transport=polling"),
];

struct Node {
    _root: tempfile::TempDir,
    dir: PrivateDir,
    state: Arc<AppState>,
}

impl Node {
    fn new() -> Self {
        let (root, dir) = private_dir();
        let room = Arc::new(Room::load(dir.clone()).unwrap());
        let state = app_state(&dir, room, "fixture-host");
        Self {
            _root: root,
            dir,
            state,
        }
    }

    fn tcp(&self) -> Router {
        router(self.state.clone(), false)
    }

    fn socket(&self) -> Router {
        router(self.state.clone(), true)
    }

    /// A device paired with a one-time code, as a phone or a browser would be.
    async fn paired(&self) -> (String, String) {
        let code = self.state.issue_code();
        let secret = code["payload"]["secret"].as_str().unwrap();
        let (status, _, body) = send(
            &self.tcp(),
            "POST",
            "/api/device/pair",
            &[],
            Some(json!({"secret": secret, "name": "Mi portátil"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (
            body["device_id"].as_str().unwrap().to_owned(),
            body["token"].as_str().unwrap().to_owned(),
        )
    }

    /// The desktop app, paired through the socket with no code.
    async fn paired_app(&self, name: &str) -> Value {
        let (status, _, body) = send(
            &self.socket(),
            "POST",
            "/api/device/local/pair",
            &[],
            Some(json!({"name": name})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    fn devices_on_disk(&self) -> Map<String, Value> {
        self.dir
            .read_json("devices.json")
            .unwrap()
            .and_then(|saved| saved["devices"].as_object().cloned())
            .unwrap_or_default()
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        request = request.header("host", "127.0.0.1:8768");
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, headers, value)
}

fn allowed_origin(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .and_then(|value| value.to_str().ok())
}

// --- What only the socket serves -------------------------------------------------------------

#[tokio::test]
async fn what_only_the_socket_serves_does_not_exist_over_tcp() {
    let node = Node::new();
    let (_, token) = node.paired().await;
    let authorization = bearer(&token);
    for (method, path) in LOCAL_PATHS {
        let body = (method == "POST").then(|| json!({"name": "x"}));
        let (status, _, answer) = send(&node.tcp(), method, path, &[], body.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
        assert!(answer["detail"].is_string(), "what an unknown route gets");
        let (status, _, _) = send(
            &node.tcp(),
            method,
            path,
            &[("authorization", &authorization)],
            body,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not even with a token");
    }
    for path in [
        "/api//local/health",
        "/api/./local/health",
        "/api/device//local",
    ] {
        let (status, _, _) = send(&node.tcp(), "GET", path, &[], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let kinds: Vec<_> = node
        .devices_on_disk()
        .values()
        .map(|row| row["kind"].clone())
        .collect();
    assert_eq!(kinds, [json!("code")], "nothing over TCP paired the app");
}

#[tokio::test]
async fn a_page_reaching_the_socket_gets_none_of_it() {
    let node = Node::new();
    for (method, path) in LOCAL_PATHS {
        let body = (method == "POST").then(|| json!({"name": "x"}));
        let (status, _, _) = send(&node.socket(), method, path, &[("origin", APP)], body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    assert!(node.devices_on_disk().is_empty(), "nothing was paired");
}

#[tokio::test]
async fn through_the_socket_the_link_is_open_it_carries_its_own_credential() {
    let node = Node::new();
    let (status, _, _) = send(
        &node.socket(),
        "GET",
        "/api/connectors/link/?EIO=4&transport=polling",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_health_probe_says_which_launch_and_what_it_is() {
    let node = Node::new();
    let (status, _, health) = send(&node.socket(), "GET", "/api/local/health", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    let mut keys: Vec<_> = health.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "api",
            "calls",
            "fingerprint",
            "host",
            "launch_id",
            "pid",
            "public_key",
            "version"
        ]
    );
    assert_eq!(health["launch_id"], "fixture");
    assert_eq!(health["pid"], std::process::id());
    assert_eq!((&health["api"], &health["calls"]), (&json!(1), &json!(0)));
    assert_eq!(
        health["fingerprint"],
        node.state.identity.fingerprint.as_str()
    );
    assert_eq!(
        health["public_key"],
        node.state.identity.public_key.as_str()
    );
    assert_eq!(health["host"], "fixture-host");
}

#[tokio::test]
async fn the_app_pairs_with_no_code_and_unpairs_itself() {
    let node = Node::new();
    let paired = node.paired_app("Sidevoice (app)").await;
    let mut keys: Vec<_> = paired.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["device_id", "node", "token"]);
    assert_eq!(paired["node"], node.state.node());
    let token = paired["token"].as_str().unwrap();
    let authorization = bearer(token);
    let (status, _, listing) = send(
        &node.socket(),
        "GET",
        "/api/device/devices",
        &[("authorization", &authorization)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows: Vec<_> = listing["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["id"].clone(),
                row["name"].clone(),
                row["kind"].clone(),
                row["current"].clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [(
            paired["device_id"].clone(),
            json!("Sidevoice (app)"),
            json!("local"),
            json!(true)
        )]
    );
    let (status, _, _) = send(
        &node.tcp(),
        "GET",
        "/api/device/devices",
        &[("authorization", &authorization)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the token works over TCP too");
    let (status, _, answer) = send(&node.socket(), "DELETE", "/api/device/local", &[], None).await;
    assert_eq!(
        (status, answer),
        (StatusCode::OK, json!({"ok": true, "revoked": true}))
    );
    let (status, _, _) = send(
        &node.socket(),
        "GET",
        "/api/device/devices",
        &[("authorization", &authorization)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, answer) = send(&node.socket(), "DELETE", "/api/device/local", &[], None).await;
    assert_eq!(
        (status, answer),
        (StatusCode::OK, json!({"ok": true, "revoked": false}))
    );
}

#[tokio::test]
async fn every_other_route_on_the_socket_still_wants_a_token() {
    let node = Node::new();
    for path in [
        "/api/presentation/admission",
        "/api/device/devices",
        "/api/connectors",
    ] {
        for headers in [&[][..], &[("authorization", "Bearer guessed")]] {
            let (status, _, _) = send(&node.socket(), "GET", path, headers, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        }
    }
    let token = node.paired_app("app").await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _, _) = send(
        &node.socket(),
        "GET",
        "/api/presentation/admission",
        &[("authorization", &bearer(&token))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, open) = send(&node.socket(), "GET", "/api/rendezvous", &[], None).await;
    assert_eq!(
        (status, &open["api"]),
        (StatusCode::OK, &json!(1)),
        "and the open ones stay open"
    );
}

// --- Device tokens over TCP ------------------------------------------------------------------

#[tokio::test]
async fn every_route_but_the_open_ones_needs_a_device_token() {
    let node = Node::new();
    let (_, token) = node.paired().await;
    let basic = format!("Basic {token}");
    let protected: [(&str, &str, Option<Value>); 12] = [
        ("GET", "/api/presentation/admission", None),
        ("GET", "/api/connectors", None),
        (
            "POST",
            "/api/rendezvous/pair",
            Some(json!({"room": "https://room.example", "code": "ABCD"})),
        ),
        ("GET", "/api/presentation/rtc/config", None),
        ("GET", "/api/device/devices", None),
        ("DELETE", "/api/device/devices/nobody", None),
        ("GET", "/api/presentation/history", None),
        ("GET", "/api/presentation/integrations", None),
        (
            "PUT",
            "/api/presentation/integrations/openai",
            Some(json!({"key": "sk-guessed"})),
        ),
        ("DELETE", "/api/presentation/integrations/openai", None),
        (
            "POST",
            "/api/models/check",
            Some(json!({"stage": "stt", "place": "openai", "model": "whisper-1"})),
        ),
    ];
    for (method, path, body) in &protected {
        for credential in [
            None,
            Some("Bearer not-a-token"),
            Some(basic.as_str()),
            Some("Bearer"),
        ] {
            let mut headers = vec![("origin", APP)];
            if let Some(value) = credential {
                headers.push(("authorization", value));
            }
            let (status, answer_headers, answer) =
                send(&node.tcp(), method, path, &headers, body.clone()).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} {credential:?}"
            );
            assert!(answer["detail"].is_string());
            assert_eq!(answer_headers[header::WWW_AUTHENTICATE], "Bearer");
            assert_eq!(
                allowed_origin(&answer_headers),
                Some(APP),
                "the page can read why"
            );
        }
    }
    let authorization = bearer(&token);
    let with_token = [("authorization", authorization.as_str())];
    for path in [
        "/api/presentation/admission",
        "/api/connectors",
        "/api/presentation/rtc/config",
    ] {
        let (status, _, _) = send(&node.tcp(), "GET", path, &with_token, None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    let lowercase = format!("bearer {token}");
    let (status, _, _) = send(
        &node.tcp(),
        "GET",
        "/api/device/devices",
        &[("authorization", &lowercase)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the scheme is case-insensitive");
    let (status, _, _) = send(
        &node.tcp(),
        "POST",
        "/api/rendezvous/pair",
        &[("origin", APP), ("authorization", &authorization)],
        Some(json!({"room": "https://room.example", "code": "ABCD"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "past the token: no connector here to pair with"
    );
    // Open: what this is, the redemption, the identity proof, preflights.
    let (status, _, what) = send(&node.tcp(), "GET", "/api/rendezvous", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        what,
        json!({"kind": "node", "fingerprint": node.state.identity.fingerprint, "api": 1})
    );
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([3u8; 16]);
    let (status, _, proof) = send(
        &node.tcp(),
        "GET",
        &format!("/api/device/identity?nonce={nonce}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut keys: Vec<_> = proof.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["fingerprint", "public_key", "signature"],
        "no host name before a code is redeemed"
    );
    let (status, _, _) = send(
        &node.tcp(),
        "GET",
        "/api/device/identity?nonce=short",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = send(
        &node.tcp(),
        "POST",
        "/api/device/pair",
        &[],
        Some(json!({"secret": "nope"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "refused for the secret, not the token"
    );
    let (status, headers, _) = send(
        &node.tcp(),
        "OPTIONS",
        "/api/presentation/text",
        &[
            ("origin", APP),
            ("access-control-request-method", "POST"),
            (
                "access-control-request-headers",
                "authorization, content-type",
            ),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(headers[header::ACCESS_CONTROL_ALLOW_HEADERS]
        .to_str()
        .unwrap()
        .contains("authorization"));
    let (status, _, _) = send(
        &node.tcp(),
        "GET",
        "/api/presentation/admission",
        &[
            ("host", "attacker.example"),
            ("authorization", &authorization),
        ],
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::MISDIRECTED_REQUEST,
        "a foreign Host goes first"
    );
}

#[tokio::test]
async fn a_code_is_redeemed_once() {
    let node = Node::new();
    let code = node.state.issue_code();
    let secret = code["payload"]["secret"].as_str().unwrap();
    let (status, _, redeemed) = send(
        &node.tcp(),
        "POST",
        "/api/device/pair",
        &[],
        Some(json!({"secret": secret, "name": "Mi portátil"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut keys: Vec<_> = redeemed["node"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(keys, ["fingerprint", "host", "public_key"]);
    assert_eq!(
        redeemed["node"]["fingerprint"], code["payload"]["fp"],
        "what a client checks before keeping the token"
    );
    let (status, _, again) = send(
        &node.tcp(),
        "POST",
        "/api/device/pair",
        &[],
        Some(json!({"secret": secret, "name": "otra"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(again["detail"].is_string());
}

#[test]
fn a_code_says_where_the_node_answers() {
    let node = Node::new();
    let payload = node.state.issue_code()["payload"].clone();
    assert_eq!(payload["urls"][0], "http://127.0.0.1:8768");
    assert_eq!(payload["rv"], Value::Null, "no rendezvous, no room");
    assert_eq!(payload["host"], "fixture-host");
}

// --- Desktop shell: CORS and Host ------------------------------------------------------------

#[tokio::test]
async fn a_desktop_shell_gets_cors_and_a_stranger_does_not() {
    let node = Node::new();
    let (status, headers, answer) = send(
        &node.tcp(),
        "GET",
        "/api/rendezvous",
        &[("origin", APP)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["kind"], "node");
    assert_eq!(allowed_origin(&headers), Some(APP));
    assert!(headers[header::VARY].to_str().unwrap().contains("Origin"));
    for windows in ["http://tauri.localhost", "https://tauri.localhost"] {
        let (_, headers, _) = send(
            &node.tcp(),
            "GET",
            "/api/rendezvous",
            &[("origin", windows)],
            None,
        )
        .await;
        assert_eq!(allowed_origin(&headers), Some(windows));
    }
    let (_, headers, _) = send(
        &node.tcp(),
        "GET",
        "/api/rendezvous",
        &[("origin", "https://evil.example")],
        None,
    )
    .await;
    assert_eq!(allowed_origin(&headers), None);
    // The origin check is what refuses the stranger's page; CORS only lets the shell read answers.
    let (_, token) = node.paired().await;
    let authorization = bearer(&token);
    let (status, _, _) = send(
        &node.tcp(),
        "GET",
        "/api/connectors",
        &[
            ("origin", "https://evil.example"),
            ("authorization", &authorization),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(
        &node.tcp(),
        "GET",
        "/api/connectors",
        &[("origin", APP), ("authorization", &authorization)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn preflights_are_answered_for_accepted_origins_only() {
    let node = Node::new();
    let (status, headers, _) = send(
        &node.tcp(),
        "OPTIONS",
        "/api/presentation/text",
        &[
            ("origin", APP),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type"),
            ("access-control-request-private-network", "true"),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(allowed_origin(&headers), Some(APP));
    assert!(headers[header::ACCESS_CONTROL_ALLOW_METHODS]
        .to_str()
        .unwrap()
        .contains("POST"));
    assert!(headers[header::ACCESS_CONTROL_ALLOW_HEADERS]
        .to_str()
        .unwrap()
        .contains("content-type"));
    assert_eq!(headers["access-control-allow-private-network"], "true");
    let (_, headers, _) = send(
        &node.tcp(),
        "OPTIONS",
        "/api/presentation/text",
        &[
            ("origin", "https://evil.example"),
            ("access-control-request-method", "POST"),
        ],
        None,
    )
    .await;
    assert_eq!(allowed_origin(&headers), None);
    assert!(!headers.contains_key("access-control-allow-private-network"));
}

#[tokio::test]
async fn a_configured_origin_gets_cors_too() {
    let node = Node::new();
    std::env::set_var("SIDEVOICE_ALLOWED_ORIGINS", "https://shell.example");
    let (_, headers, _) = send(
        &node.tcp(),
        "GET",
        "/api/rendezvous",
        &[("origin", "https://shell.example")],
        None,
    )
    .await;
    assert_eq!(allowed_origin(&headers), Some("https://shell.example"));
}

#[tokio::test]
async fn a_foreign_host_is_still_refused_first() {
    let node = Node::new();
    for host in [
        "attacker.example",
        "attacker.example:8768",
        "127.0.0.1.evil.example",
    ] {
        let (status, _, _) = send(
            &node.tcp(),
            "GET",
            "/api/rendezvous",
            &[("host", host), ("origin", APP)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{host}");
    }
    let (status, _, _) = send(
        &node.socket(),
        "GET",
        "/api/local/health",
        &[("host", "attacker.example")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "on the socket too");
}

#[tokio::test]
async fn pairing_a_room_is_a_page_s_act_not_a_script_s() {
    let node = Node::new();
    let (_, token) = node.paired().await;
    let authorization = bearer(&token);
    let body = json!({"room": " https://room.example ", "code": " ABCD-1234 "});
    for origin in [None, Some("https://evil.example")] {
        let mut headers = vec![("authorization", authorization.as_str())];
        if let Some(origin) = origin {
            headers.push(("origin", origin));
        }
        let (status, _, _) = send(
            &node.tcp(),
            "POST",
            "/api/rendezvous/pair",
            &headers,
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin:?}");
    }
    let page = [("origin", APP), ("authorization", authorization.as_str())];
    let (status, headers, answer) = send(
        &node.tcp(),
        "POST",
        "/api/rendezvous/pair",
        &page,
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no connector linked"
    );
    assert!(answer["detail"].is_string());
    assert_eq!(allowed_origin(&headers), Some(APP));
    let (status, _, _) = send(
        &node.tcp(),
        "POST",
        "/api/rendezvous/pair",
        &page,
        Some(json!({"room": "", "code": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// --- Integrations: routes that never reach a provider ----------------------------------------

#[tokio::test]
async fn integration_writes_come_from_a_page_and_name_a_known_provider_and_a_key() {
    let node = Node::new();
    let (_, token) = node.paired().await;
    let authorization = bearer(&token);
    let page = [
        ("origin", "http://127.0.0.1:8768"),
        ("authorization", authorization.as_str()),
    ];
    let script = [("authorization", authorization.as_str())];
    let (status, _, _) = send(
        &node.tcp(),
        "PUT",
        "/api/presentation/integrations/openai",
        &script,
        Some(json!({"key": "sk-script"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no Origin, no write");
    let (status, _, _) = send(
        &node.tcp(),
        "PUT",
        "/api/presentation/integrations/nobody",
        &page,
        Some(json!({"key": "sk-x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for body in [json!({"key": "   "}), json!({}), json!({"key": 7})] {
        let (status, _, _) = send(
            &node.tcp(),
            "PUT",
            "/api/presentation/integrations/openai",
            &page,
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    assert_eq!(node.dir.read_json("integrations.json").unwrap(), None);
}

#[tokio::test]
async fn a_key_the_provider_cannot_take_is_not_stored_and_the_old_one_stays() {
    let node = Node::new();
    node.dir
        .write_json("integrations.json", &json!({"openai": "sk-installed-0000"}))
        .unwrap();
    let (_, token) = node.paired().await;
    let authorization = bearer(&token);
    let page = [
        ("origin", "http://127.0.0.1:8768"),
        ("authorization", authorization.as_str()),
    ];
    // Not a value an Authorization header can carry: refused before any request is made.
    let (status, _, _) = send(
        &node.tcp(),
        "PUT",
        "/api/presentation/integrations/openai",
        &page,
        Some(json!({"key": "sk-bad\nkey"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        node.dir.read_json("integrations.json").unwrap().unwrap(),
        json!({"openai": "sk-installed-0000"})
    );
}

#[tokio::test]
async fn the_listing_never_carries_a_key_and_delete_removes_only_the_saved_one() {
    let node = Node::new();
    node.dir
        .write_json(
            "integrations.json",
            &json!({"openai": "sk-openai-1234", "elevenlabs": "xi-voice-5678"}),
        )
        .unwrap();
    let (_, token) = node.paired().await;
    let authorization = bearer(&token);
    let page = [
        ("origin", "http://127.0.0.1:8768"),
        ("authorization", authorization.as_str()),
    ];
    let (status, _, listing) = send(
        &node.tcp(),
        "GET",
        "/api/presentation/integrations",
        &page,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = listing.to_string();
    assert!(!text.contains("sk-openai-1234") && !text.contains("xi-voice-5678"));
    let ids: Vec<_> = listing["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["id"].clone(),
                row["source"].clone(),
                row["hint"].clone(),
            )
        })
        .collect();
    assert_eq!(
        ids,
        [
            (json!("openai"), json!("stored"), json!("…1234")),
            (json!("elevenlabs"), json!("stored"), json!("…5678"))
        ]
    );
    let (status, _, _) = send(
        &node.tcp(),
        "DELETE",
        "/api/presentation/integrations/openai",
        &page,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        node.dir.read_json("integrations.json").unwrap().unwrap(),
        json!({"elevenlabs": "xi-voice-5678"}),
        "the other provider's key stays"
    );
    assert_eq!(
        std::fs::metadata(node.dir.path().join("integrations.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

// --- The call socket over a real listener ----------------------------------------------------

async fn serve(app: Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn call(
    address: std::net::SocketAddr,
    protocols: Option<&str>,
    authorization: Option<&str>,
) -> (Client, Option<String>) {
    let mut request = format!("ws://{address}/api/presentation/ws")
        .into_client_request()
        .unwrap();
    if let Some(protocols) = protocols {
        request
            .headers_mut()
            .insert("sec-websocket-protocol", protocols.parse().unwrap());
    }
    if let Some(value) = authorization {
        request
            .headers_mut()
            .insert("authorization", value.parse().unwrap());
    }
    let (socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    let accepted = response
        .headers()
        .get("sec-websocket-protocol")
        .map(|value| value.to_str().unwrap().to_owned());
    (socket, accepted)
}

async fn close_code(socket: &mut Client) -> u16 {
    let deadline = std::time::Duration::from_secs(10);
    loop {
        match tokio::time::timeout(deadline, socket.next())
            .await
            .expect("closed in time")
        {
            Some(Ok(tungstenite::Message::Close(Some(frame)))) => return frame.code.into(),
            Some(Ok(tungstenite::Message::Close(None))) | None => return 1005,
            Some(Ok(_)) => continue,
            Some(Err(error)) => panic!("{error}"),
        }
    }
}

async fn until(check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("timed out waiting");
}

#[tokio::test]
async fn the_call_socket_takes_its_token_as_a_subprotocol() {
    let node = Node::new();
    let (_, token) = node.paired().await;
    let address = serve(node.tcp()).await;
    for offered in [
        Some("sidevoice"),
        Some("sidevoice, sidevoice.token.guessed"),
        None,
    ] {
        let (mut socket, accepted) = call(address, offered, None).await;
        if offered.is_some() {
            assert_eq!(
                accepted.as_deref(),
                Some("sidevoice"),
                "accepted, then closed: a page learns it must pair again"
            );
        }
        assert_eq!(close_code(&mut socket).await, 4401, "{offered:?}");
    }
    let (mut socket, accepted) = call(address, Some("sidevoice"), Some(&bearer(&token))).await;
    assert_eq!(accepted.as_deref(), Some("sidevoice"));
    assert_eq!(
        close_code(&mut socket).await,
        4401,
        "the socket takes its token as a subprotocol, as a browser must send it"
    );
    let offered = format!("sidevoice, sidevoice.token.{token}");
    let (_socket, accepted) = call(address, Some(&offered), None).await;
    assert_eq!(accepted.as_deref(), Some("sidevoice"));
    until(|| node.state.open_calls() == 1).await;
}

#[tokio::test]
async fn a_call_socket_counts_from_its_acceptance_to_its_close() {
    let node = Node::new();
    let token = node.paired_app("app").await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let address = serve(node.socket()).await;
    let offered = format!("sidevoice, sidevoice.token.{token}");
    let (mut socket, _) = call(address, Some(&offered), None).await;
    until(|| node.state.open_calls() == 1).await;
    let (_, _, health) = send(&node.socket(), "GET", "/api/local/health", &[], None).await;
    assert_eq!(health["calls"], 1, "no hello sent, still a call");
    socket.close(None).await.unwrap();
    until(|| node.state.open_calls() == 0).await;
}

#[tokio::test]
async fn pairing_the_app_again_or_removing_it_ends_its_call() {
    let node = Node::new();
    let address = serve(node.socket()).await;
    let first = node.paired_app("first").await;
    let offered = format!(
        "sidevoice, sidevoice.token.{}",
        first["token"].as_str().unwrap()
    );
    let (mut socket, _) = call(address, Some(&offered), None).await;
    until(|| node.state.open_calls() == 1).await;
    node.paired_app("second").await;
    assert_eq!(close_code(&mut socket).await, 4401, "its call ends now");
    until(|| node.state.open_calls() == 0).await;

    let third = node.paired_app("third").await;
    let offered = format!(
        "sidevoice, sidevoice.token.{}",
        third["token"].as_str().unwrap()
    );
    let (mut socket, _) = call(address, Some(&offered), None).await;
    until(|| node.state.open_calls() == 1).await;
    let (status, _, _) = send(&node.socket(), "DELETE", "/api/device/local", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(close_code(&mut socket).await, 4401);
    until(|| node.state.open_calls() == 0).await;
}

#[tokio::test]
async fn revoking_a_device_ends_the_call_it_has_open_and_the_list_marks_the_asker() {
    let node = Node::new();
    let (phone, phone_token) = node.paired().await;
    let (laptop, laptop_token) = node.paired().await;
    let address = serve(node.tcp()).await;
    let offered = format!("sidevoice, sidevoice.token.{phone_token}");
    let (mut socket, _) = call(address, Some(&offered), None).await;
    until(|| node.state.open_calls() == 1).await;
    let authorization = bearer(&laptop_token);
    let (_, _, listing) = send(
        &node.tcp(),
        "GET",
        "/api/device/devices",
        &[("authorization", &authorization)],
        None,
    )
    .await;
    let current: Vec<_> = listing["devices"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["current"] == true)
        .map(|row| row["id"].clone())
        .collect();
    assert_eq!(current, [json!(laptop)]);
    let (status, _, _) = send(
        &node.tcp(),
        "DELETE",
        &format!("/api/device/devices/{phone}"),
        &[("authorization", &authorization)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(close_code(&mut socket).await, 4401);
    let (status, _, _) = send(
        &node.tcp(),
        "GET",
        "/api/device/devices",
        &[("authorization", &bearer(&phone_token))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoking is immediate");
}
