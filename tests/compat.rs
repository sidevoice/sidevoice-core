//! This core against the latest published releases of the pieces that talk to it. `cargo xtask compat` downloads
//! and verifies each release, then runs these with the unpacked artifact named in the environment:
//!
//! - `SIDEVOICE_COMPAT_CONNECTOR`: the released connector's program (`bin/sidevoice-connector`). Its daemon
//!   finds this core serving where it looks for its own (`<data>/core`), links to it as to a core it did not
//!   start; a conversation joins through its MCP server with a local HTTP receiver standing in for the agent; a
//!   browser's typed input is delivered to it, and its reply (`voice_say`) reaches the browser.
//! - `SIDEVOICE_COMPAT_WEB`: the released web client's static site. Every `/api/…` route its bundle names exists
//!   on this core. Until sidevoice-web publishes a release there is none: `cargo xtask compat` says so and this
//!   test is not run.
//!
//! Ignored by `cargo test`: they need the artifacts.

mod support;

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use serde_json::{json, Value};
use support::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn artifact(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name).map(PathBuf::from);
    if value.is_none() {
        eprintln!("{name} is not set: no released artifact to check against");
    }
    value
}

/// A process of the released connector, stopped (SIGTERM) when dropped.
struct Connector(Child);

impl Drop for Connector {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        let _ = self.0.wait();
    }
}

/// The released connector's MCP server over its stdio, as an agent harness runs it: one JSON-RPC message per line.
struct Mcp {
    _process: Connector,
    stdin: ChildStdin,
    lines: Receiver<String>,
    serial: u64,
}

impl Mcp {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the released connector serves MCP");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            _process: Connector(child),
            stdin,
            lines,
            serial: 0,
        }
    }

    fn send(&mut self, frame: Value) {
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// A request and its result; an error answer fails the test.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.serial += 1;
        let id = self.serial;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("the released connector gave no answer to {method}"));
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["id"] == id {
                assert!(message.get("error").is_none(), "{method}: {message}");
                return message["result"].clone();
            }
        }
    }

    /// A tool call whose text content is JSON; a tool error fails the test.
    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_ne!(result["isError"], true, "{name}: {result}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap_or("null"))
            .unwrap_or_else(|_| panic!("{name}: not JSON: {result}"))
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by `cargo xtask compat` against the released connector"]
async fn the_released_connector_links_delivers_and_publishes() {
    let Some(connector) = artifact("SIDEVOICE_COMPAT_CONNECTOR") else {
        return;
    };
    // A private profile: the connector's data directory, and homes for the agents it looks for.
    let root = tempfile::tempdir().unwrap();
    let dir = |name: &str| root.path().join(name);
    for name in ["home", "sidevoice", "claude", "codex", "cursor", "xdg"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir(name))
            .unwrap();
    }
    let profile = |command: &mut Command| {
        command.env_clear().envs([
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("HOME", dir("home")),
            ("SIDEVOICE_DATA_DIR", dir("sidevoice")),
            ("CLAUDE_CONFIG_DIR", dir("claude")),
            ("CODEX_HOME", dir("codex")),
            ("CURSOR_CONFIG_DIR", dir("cursor/config")),
            ("CURSOR_DATA_DIR", dir("cursor/data")),
            ("XDG_CONFIG_HOME", dir("xdg/config")),
            ("XDG_DATA_HOME", dir("xdg/data")),
        ]);
    };
    // This core, serving where the connector looks for its core (`<data>/core`, its socket and ready file there):
    // the connector links to a core it did not start.
    let core = Launch::new(dir("sidevoice").join("core"))
        .arg("--launch-id")
        .arg("compat")
        .start();
    let log = std::fs::File::create(dir("connector.log")).unwrap();
    let mut daemon = Command::new(&connector);
    profile(&mut daemon);
    let _daemon = Connector(
        daemon
            .arg("connector")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("the released connector runs"),
    );

    // A conversation joins through the connector's MCP server, delivered to a local HTTP receiver.
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/deliver"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;
    let thread = "compat-thread";
    let mut command = Command::new(&connector);
    profile(&mut command);
    command.arg("mcp").env("SIDEVOICE_THREAD", thread).env(
        "SIDEVOICE_DELIVERY_URL",
        format!("{}/deliver", receiver.uri()),
    );
    let mut mcp = Mcp::start(command);
    mcp.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "sidevoice-core-compat", "version": "test"}}),
    );
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}));
    let joined = mcp.tool("voice_connect", json!({"title": "Compatibility"}));
    assert_eq!(joined["conversation"], thread, "{joined}");

    let token = core.pair_local("Browser").await;
    let linked = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if core.participant(&token, thread).await["available"] == true {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    if linked.is_err() {
        let log = std::fs::read_to_string(dir("connector.log")).unwrap_or_default();
        panic!(
            "the released connector never linked its conversation to this core; its log:\n{log}"
        );
    }

    // What the person types in a call reaches the conversation through the connector.
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

    // The conversation answers through the connector: the reply reaches the call.
    let said = mcp.tool(
        "voice_say",
        json!({"session_id": delivered[0]["session_id"], "revision": delivered[0]["revision"],
            "text": "Reply through the released connector"}),
    );
    assert_eq!(said["text_saved"], true, "{said}");
    assert_eq!(
        browser.frame("voice-reply").await["text"],
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
