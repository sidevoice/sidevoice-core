//! Provider keys, verified before they are stored. The provider is a local
//! fixture, so the test runs only with the `hosted-fixtures` feature that points verification at it:
//! CI runs the suite with `--all-features`; locally, `cargo test --features hosted-fixtures --test integrations`.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use sidevoice_core::control::devices::{DeviceRegistry, NodeIdentity};
use sidevoice_core::control::room::Room;
use sidevoice_core::server::{self, rendezvous::Rendezvous, AppState};
use sidevoice_core::storage::PrivateDir;
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PAGE: &str = "http://127.0.0.1:8768";

struct Node {
    _root: tempfile::TempDir,
    dir: PrivateDir,
    app: Router,
    authorization: String,
}

impl Node {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open(root.path().join("core")).unwrap();
        let identity = NodeIdentity::load_or_create(&dir).unwrap();
        let registry = DeviceRegistry::load(dir.clone()).unwrap();
        let room = Arc::new(Room::load(dir.clone()).unwrap());
        let relay = Rendezvous::new(
            None,
            url::Url::parse("http://127.0.0.1:8768/").unwrap(),
            "fixture-host".into(),
            room.clone(),
        );
        let state = Arc::new(AppState::new(
            dir.clone(),
            identity,
            registry,
            "fixture".into(),
            "fixture-host".into(),
            8768,
            room,
            relay,
        ));
        let secret = state.issue_code()["payload"]["secret"]
            .as_str()
            .unwrap()
            .to_owned();
        let app = server::router(state, false);
        let mut node = Self {
            _root: root,
            dir,
            app,
            authorization: String::new(),
        };
        let (status, paired) = node
            .request("POST", "/api/device/pair", Some(json!({"secret": secret})))
            .await;
        assert_eq!(status, StatusCode::OK);
        node.authorization = format!("Bearer {}", paired["token"].as_str().unwrap());
        node
    }

    async fn request(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        call(self.app.clone(), &self.authorization, method, uri, body).await
    }

    /// A PUT from its own client, running on its own.
    fn put(&self, key: &str) -> tokio::task::JoinHandle<(StatusCode, Value)> {
        let (app, authorization) = (self.app.clone(), self.authorization.clone());
        let body = json!({"key": key});
        tokio::spawn(async move {
            call(
                app,
                &authorization,
                "PUT",
                "/api/presentation/integrations/openai",
                Some(body),
            )
            .await
        })
    }

    fn stored(&self) -> Value {
        self.dir
            .read_json("integrations.json")
            .unwrap()
            .unwrap_or(Value::Null)
    }
}

async fn call(
    app: Router,
    authorization: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:8768")
        .header("origin", PAGE)
        .header("content-type", "application/json")
        .header("authorization", authorization)
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// One fixture for the whole target: the verification base is read from the process environment.
async fn provider() -> MockServer {
    let server = MockServer::start().await;
    let models = json!({"object": "list", "data": []});
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer sk-slow-1111"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(models.clone())
                .set_delay(Duration::from_millis(1500)),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer sk-refused-0000"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            json!({"error": {"message": "Incorrect API key", "type": "invalid_request_error"}}),
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models))
        .mount(&server)
        .await;
    std::env::set_var("SIDEVOICE_OPENAI_FIXTURE_BASE", server.uri());
    std::env::remove_var("VOICE_STT_API_KEY");
    server
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(feature = "hosted-fixtures"),
    ignore = "needs --features hosted-fixtures to point key verification at a local fixture"
)]
async fn keys_are_verified_stored_privately_and_the_last_change_asked_for_wins() {
    let _provider = provider().await;

    // A key is verified, stored privately and never returned.
    let node = Node::new().await;
    let (status, listing) = node
        .request(
            "PUT",
            "/api/presentation/integrations/openai",
            Some(json!({"key": " sk-plain-3333 "})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    assert!(!listing.to_string().contains("sk-plain-3333"));
    assert_eq!(listing["providers"][0]["hint"], "…3333");
    assert_eq!(node.stored(), json!({"openai": "sk-plain-3333"}));
    assert_eq!(
        std::fs::metadata(node.dir.path().join("integrations.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // A key the provider refuses is not stored, and the old one stays.
    let (status, _) = node
        .request(
            "PUT",
            "/api/presentation/integrations/openai",
            Some(json!({"key": "sk-refused-0000"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(node.stored(), json!({"openai": "sk-plain-3333"}));

    // A removal while a key is being verified is not undone by it.
    let node = Node::new().await;
    node.dir
        .write_json("integrations.json", &json!({"openai": "sk-installed-0000"}))
        .unwrap();
    let pending = node.put("sk-slow-1111");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (status, _) = node
        .request("DELETE", "/api/presentation/integrations/openai", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node.stored(), json!({}));
    let (status, answer) = pending.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert!(answer["detail"].is_string() || answer["detail"].is_object());
    assert_eq!(node.stored(), json!({}), "the removal stands");

    // A newer key wins over an older one still being verified.
    let node = Node::new().await;
    let older = node.put("sk-slow-1111");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (status, _) = node.put("sk-newer-2222").await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let (status, _) = older.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(node.stored(), json!({"openai": "sk-newer-2222"}));
    // Nothing was pending this time: a key is saved as ever.
    let (status, _) = node.put("sk-plain-3333").await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node.stored(), json!({"openai": "sk-plain-3333"}));
}
