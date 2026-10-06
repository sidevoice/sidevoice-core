//! This core against the latest published releases of the pieces that talk to it. `cargo xtask compat` downloads
//! and verifies each release, then runs these with the unpacked artifact named in the environment:
//!
//! - `SIDEVOICE_COMPAT_CONNECTOR`: the released connector's `dist/cli.mjs`. Run as a user's machine runs it
//!   (`sidevoice connector`, Node 22 or later), it finds this core by its ready file, links to it, registers a
//!   conversation that a local HTTP receiver stands in for, delivers a browser's typed input to it and publishes
//!   its reply back to the browser.
//! - `SIDEVOICE_COMPAT_WEB`: the released web client's static site. Every `/api/…` route its bundle names exists
//!   on this core.
//!
//! Ignored by `cargo test`: they need the artifacts.

mod support;

use std::collections::BTreeSet;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use support::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn artifact(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name).map(PathBuf::from);
    if value.is_none() {
        eprintln!("{name} is not set: no released artifact to check against");
    }
    value
}

/// The connector's local socket, the way its own façades speak to it: one JSON object per line.
struct Facade {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next: u64,
}

impl Facade {
    async fn open(socket: &Path) -> Self {
        let stream = eventually(
            Duration::from_secs(30),
            "the connector's socket",
            || async { UnixStream::connect(socket).await.ok() },
        )
        .await;
        let (reader, writer) = stream.into_split();
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
            next: 0,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let line = json!({"id": self.next, "method": method, "params": params}).to_string() + "\n";
        self.writer.write_all(line.as_bytes()).await.unwrap();
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("the connector never answered {method}"))
                .unwrap()
                .expect("the connector closed its socket");
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == self.next {
                assert_eq!(reply["ok"], true, "{method}: {reply}");
                return reply["result"].clone();
            }
        }
    }
}

struct Connector(std::process::Child);

impl Drop for Connector {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by `cargo xtask compat` against the released connector"]
async fn the_released_connector_links_delivers_and_publishes() {
    let Some(cli) = artifact("SIDEVOICE_COMPAT_CONNECTOR") else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let data = root.path().join("sidevoice");
    for dir in [&home, &data] {
        std::fs::DirBuilder::new().mode(0o700).create(dir).unwrap();
    }
    let core = Launch::new(data.join("core")).start();
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/deliver"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;
    let log = std::fs::File::create(root.path().join("connector.log")).unwrap();
    let _connector = Connector(
        Command::new("node")
            .arg(&cli)
            .arg("connector")
            .env("HOME", &home)
            .env("SIDEVOICE_DATA_DIR", &data)
            .env("SIDEVOICE_CORE_BIN", CORE)
            .env_remove("SIDEVOICE_URL")
            .env_remove("SIDEVOICE_CONNECTOR_SOCKET")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("node runs the released connector"),
    );
    let mut facade = Facade::open(&data.join("connector.sock")).await;
    let thread = "compat-thread";
    facade
        .call(
            "register",
            json!({"client_ref": "compat", "harness": "http", "thread": thread, "title": "Compatibility",
                "delivery": {"kind": "http", "url": format!("{}/deliver", receiver.uri()), "thread": thread}}),
        )
        .await;
    let token = core.pair_local("Browser").await;
    let linked = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if core.participant(&token, thread).await["available"] == true {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    if linked.is_err() {
        let log = std::fs::read_to_string(root.path().join("connector.log")).unwrap_or_default();
        panic!(
            "the released connector never linked its conversation to this core; its log:\n{log}"
        );
    }

    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let focus = core.select(&token, &session, thread).await;
    let sent = core
        .text(
            &token,
            (&session, thread, &focus),
            "Typed into the released connector",
            &message_id(),
        )
        .await;
    browser.receipt("pending").await;
    assert_eq!(browser.receipt("delivered").await["history_id"], sent["id"]);
    let delivered: Vec<Value> = receiver
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0]["text"], "Typed into the released connector");
    assert_eq!(delivered[0]["thread_id"], thread);

    let revision = core.revision(&token, &session).await;
    let published = facade
        .call(
            "publish",
            json!({"client_ref": "compat", "session_id": session, "revision": revision,
                "text": "Reply through the released connector"}),
        )
        .await;
    assert_ne!(published["status"], "rejected", "{published}");
    assert_eq!(
        browser.frame("voice-speech").await["text"],
        "Reply through the released connector"
    );
}

/// The paths a page asks of a node (this core). The rest of `/api/` is the room's own (`/api/connectors…`,
/// `/api/telemetry`), never a node's.
const NODE: [&str; 4] = [
    "/api/presentation",
    "/api/device",
    "/api/models",
    "/api/rendezvous",
];

/// Every node path written in the bundle's JavaScript; one ending in `/` is a prefix the bundle appends ids to.
fn routes(site: &Path) -> BTreeSet<String> {
    fn scripts(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scripts(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "js") {
                found.push(path);
            }
        }
    }
    let mut files = Vec::new();
    scripts(&site.join("voice"), &mut files);
    assert!(
        !files.is_empty(),
        "the release has the page's scripts under voice/"
    );
    let mut routes = BTreeSet::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for (at, _) in text.match_indices("/api/") {
            let route: String = text[at..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-'))
                .collect();
            if NODE
                .iter()
                .any(|prefix| route == *prefix || route.starts_with(&format!("{prefix}/")))
            {
                routes.insert(route);
            }
        }
    }
    routes
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by `cargo xtask compat` against the released web client"]
async fn every_route_the_released_web_client_names_exists() {
    let Some(site) = artifact("SIDEVOICE_COMPAT_WEB") else {
        return;
    };
    let routes = routes(&site);
    assert!(
        routes.contains("/api/presentation/ws"),
        "the bundle opens the call socket: {routes:?}"
    );
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let messages: Value = serde_json::from_slice(
        &std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("rust/messages/en.json"))
            .unwrap(),
    )
    .unwrap();
    let unknown = messages["request.not_found"].clone();
    let mut missing = Vec::new();
    for route in &routes {
        // A route the core does not have falls through to its fallback; a route it has may still answer 404
        // about what it was asked (no such session), in its own words. A prefix stands for one or two ids.
        let candidates = if route.ends_with('/') {
            vec![format!("{route}x"), format!("{route}x/x")]
        } else {
            vec![route.clone()]
        };
        let mut known = false;
        for candidate in &candidates {
            for reply in [
                core.get(candidate).token(&token).send().await,
                core.local("GET", candidate).token(&token).send().await,
            ] {
                let fallback = reply.status == 404
                    && serde_json::from_slice::<Value>(&reply.body)
                        .is_ok_and(|body| body["detail"] == unknown);
                known |= !fallback;
            }
        }
        if !known {
            missing.push(route.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "routes the released web client calls and this core does not serve: {missing:?}"
    );
}
