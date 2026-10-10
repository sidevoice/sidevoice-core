//! What the core keeps on disk and says about itself, driven as the real process: the requests that must not
//! pass for local or authenticated ones, its log, and the state it repairs at start, a broken connector
//! credential, or ignores, an obsolete room history. Command line, identity proof, log rotation and the trust
//! boundary are `runtime`, `devices` and `server` unit tests and `node_process`.

mod support;

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use serde_json::{json, Value};
use support::*;

fn private_dir(path: &Path) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_upgrade_or_spoofed_local_headers_grant_nothing() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Local app").await;
    let devices = core
        .get("/api/device/devices")
        .token(&token)
        .send()
        .await
        .json();
    let device = devices["devices"][0]["id"].as_str().unwrap().to_owned();
    let registry = core.data.join("devices.json");
    let before = fs::read(&registry).unwrap();
    let forged = core
        .http("DELETE", &format!("/api/device/devices/{device}"))
        .header("upgrade", "websocket")
        .send()
        .await;
    assert_eq!(forged.status, 401, "{forged:?}");
    assert_eq!(forged.header("www-authenticate"), Some("Bearer"));
    assert_eq!(
        forged
            .json()
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["detail"]
    );
    assert_eq!(
        fs::read(&registry).unwrap(),
        before,
        "a forged Upgrade changed nothing"
    );

    for (method, path) in [
        ("GET", "/api/local/health"),
        ("POST", "/api/device/local/pair"),
        ("DELETE", "/api/device/local"),
    ] {
        let spoofed = core
            .http(method, path)
            .header("sidevoice.local", "true")
            .header("x-sidevoice-local", "true")
            .header("x-forwarded-for", "127.0.0.1")
            .header("x-forwarded-proto", "http")
            .json(json!({"name": "spoofed"}))
            .send()
            .await;
        assert_eq!(spoofed.status, 404, "{method} {path} over TCP: {spoofed:?}");
        assert_eq!(
            spoofed
                .json()
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["detail"]
        );
    }
    assert_eq!(
        core.get("/api/device/devices")
            .token(&token)
            .send()
            .await
            .status,
        200
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_never_names_the_room_credential() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    private_dir(&logs);
    let log = logs.join("custom.log");
    let credential = root.path().join("room-credentials.json");
    let mut core = Launch::new(root.path().join("core"))
        .arg("--log-file")
        .arg(log.display().to_string())
        .arg("--room-credential")
        .arg(credential.display().to_string())
        .start();
    let text = until(STEP, "the ready line in the log", || {
        let text = fs::read_to_string(&log).ok()?;
        text.contains("runtime.log_ready").then_some(text)
    });
    assert!(text.contains("runtime.log_start"), "{text}");
    assert!(
        !text.contains("room-credentials.json"),
        "the log names no credential: {text}"
    );
    assert_eq!(core.stop(), 0);
}
#[tokio::test(flavor = "multi_thread")]
async fn a_broken_connector_credential_is_replaced_and_nothing_else_is() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    let mut core = Launch::new(&data).start();
    let first = (core.connector_id(), core.connector_token());
    assert_eq!(core.stop(), 0);
    let room = read_json(&data.join("room-state.json"));
    let identity = fs::read(data.join("node-identity.json")).unwrap();

    fs::write(data.join("connector-credential.json"), "{invalid-json").unwrap();
    let mut core = Launch::new(&data).start();
    let second = (core.connector_id(), core.connector_token());
    assert_ne!(
        second, first,
        "a credential that cannot be read is replaced"
    );
    let saved = read_json(&data.join("connector-credential.json"));
    assert_eq!(
        (saved["connector_id"].as_str(), saved["token"].as_str()),
        (Some(second.0.as_str()), Some(second.1.as_str()))
    );
    assert_eq!(mode(&data.join("connector-credential.json")), 0o600);
    let after = read_json(&data.join("room-state.json"));
    assert_eq!(
        after["connectors"][&first.0], room["connectors"][&first.0],
        "the old pairing is kept"
    );
    assert!(after["connectors"].get(&second.0).is_some());
    assert_eq!(
        fs::read(data.join("node-identity.json")).unwrap(),
        identity,
        "the node keeps its identity"
    );
    assert_eq!(core.stop(), 0);
}

/// A room history from before the room kept its state as JSON is not read: whatever is in it, the core starts with an
/// empty room, and no connector it named can link.
#[tokio::test(flavor = "multi_thread")]
async fn an_obsolete_room_history_neither_stops_the_start_nor_reaches_the_room() {
    let root = tempfile::tempdir().unwrap();
    for (name, history) in [
        ("garbage", b"not a sqlite database".to_vec()),
        ("database", b"SQLite format 3\0".to_vec()),
    ] {
        let data = root.path().join(name);
        private_dir(&data);
        fs::write(data.join("room-history.sqlite3"), history).unwrap();
        let mut core = Launch::new(&data).start();
        let own = core.connector_id();
        assert_eq!(core.stop(), 0, "{name}");
        // The only connector the room knows is the one this core made for its own machine.
        let connectors = read_json(&data.join("room-state.json"))["connectors"].clone();
        let known: Vec<_> = connectors.as_object().unwrap().keys().cloned().collect();
        assert_eq!(known, [own], "{name}");
    }
}
