//! What the core keeps on disk and says about itself, driven as the real process: its command line, the identity
//! it proves, the requests that must not pass for local or authenticated ones, its log, and the state it repairs
//! or imports at start — a broken connector credential, a legacy room history.

mod support;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::pkcs8::DecodePublicKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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

fn failure_key(data: &Path) -> Value {
    read_json(&data.join("core-failure.json"))["key"].clone()
}

#[test]
fn help_lists_the_options_and_an_unreadable_argument_is_refused() {
    let help = Command::new(CORE)
        .arg("--help")
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    for option in ["Usage:", "--log-file", "--room-credential", "--idle-exit"] {
        assert!(text.contains(option), "--help names {option}: {text}");
    }
    let refused = Command::new(CORE)
        .arg(OsStr::from_bytes(b"\xff"))
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("Invalid core command-line arguments")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_node_proves_the_identity_it_names() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let health = core.local("GET", "/api/local/health").send().await.json();
    let fingerprint = health["fingerprint"].as_str().unwrap().to_owned();
    let public_key = health["public_key"].as_str().unwrap().to_owned();
    assert_eq!(
        core.get("/api/rendezvous").send().await.json(),
        json!({"kind": "node", "fingerprint": fingerprint, "api": 1})
    );
    let der = STANDARD.decode(&public_key).unwrap();
    assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(&der)), fingerprint);
    let key = VerifyingKey::from_public_key_der(&der).expect("a P-256 public key");
    let nonce = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let proof = core
        .get(&format!("/api/device/identity?nonce={nonce}"))
        .send()
        .await;
    assert_eq!(proof.status, 200, "{proof:?}");
    let proof = proof.json();
    assert_eq!(
        (proof["fingerprint"].as_str(), proof["public_key"].as_str()),
        (Some(fingerprint.as_str()), Some(public_key.as_str()))
    );
    let raw = URL_SAFE_NO_PAD
        .decode(proof["signature"].as_str().unwrap().trim_end_matches('='))
        .unwrap();
    let signature = Signature::from_slice(&raw).expect("a raw 64-byte signature");
    assert!(key
        .verify(
            format!("sidevoice-node-identity:{nonce}").as_bytes(),
            &signature
        )
        .is_ok());
    assert!(key
        .verify(b"sidevoice-node-identity:different", &signature)
        .is_err());
    assert_eq!(
        core.get("/api/device/identity?nonce=bad")
            .send()
            .await
            .status,
        400
    );
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
async fn the_log_rotates_and_never_names_the_room_credential() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs");
    private_dir(&logs);
    let log = logs.join("custom.log");
    fs::write(&log, vec![b'x'; 5_000_000]).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
    let credential = root.path().join("room-credentials.json");
    let mut core = Launch::new(root.path().join("core"))
        .arg("--launch-id")
        .arg("custom-log")
        .arg("--log-file")
        .arg(log.display().to_string())
        .arg("--room-credential")
        .arg(credential.display().to_string())
        .start();
    assert_eq!(core.ready["launch_id"], "custom-log");
    assert_eq!(
        fs::metadata(logs.join("custom.log.1")).unwrap().len(),
        5_000_000
    );
    assert_eq!(mode(&log), 0o600);
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

fn legacy_history(data: &Path, rows: &[(&str, &str, &str, Value)]) {
    let db = rusqlite::Connection::open(data.join("room-history.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TABLE connectors (id TEXT, token_hash TEXT, host TEXT, created INTEGER, last_seen INTEGER, revoked INTEGER)",
    )
    .unwrap();
    for (id, token, host, created) in rows {
        let hash = format!("{:x}", Sha256::digest(token.as_bytes()));
        let created: Box<dyn rusqlite::ToSql> = match created {
            Value::Number(number) => Box::new(number.as_i64().unwrap()),
            other => Box::new(other.as_str().unwrap().to_owned()),
        };
        db.execute(
            "INSERT INTO connectors VALUES (?1, ?2, ?3, ?4, 2, 0)",
            rusqlite::params![id, hash, host, created],
        )
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_room_history_is_imported_whole_or_not_at_all() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("legacy");
    private_dir(&legacy);
    legacy_history(
        &legacy,
        &[("legacy-id", "legacy-token", "old-machine", json!(1))],
    );
    let mut core = Launch::new(&legacy).start();
    assert_eq!(core.stop(), 0);
    let kept = read_json(&legacy.join("room-state.json"))["connectors"]["legacy-id"].clone();
    assert_eq!(kept["host"], "old-machine");
    assert_eq!(
        kept["token_hash"],
        format!("{:x}", Sha256::digest(b"legacy-token"))
    );

    // A history that is not a database stops the start and leaves no state behind.
    let repair = root.path().join("repair");
    private_dir(&repair);
    fs::write(repair.join("room-history.sqlite3"), "not a sqlite database").unwrap();
    let mut broken = Launch::new(&repair).spawn();
    assert_eq!(broken.exited(Duration::from_secs(15)), Some(0));
    assert_eq!(failure_key(&repair), "start.failed");
    assert!(!repair.join("room-state.json").exists());

    // One unreadable row: nothing is imported, not even the good one.
    fs::remove_file(repair.join("room-history.sqlite3")).unwrap();
    legacy_history(
        &repair,
        &[
            ("kept-id", "kept-token", "repair-host", json!(1)),
            ("bad-id", "bad-token", "bad-host", json!("not-an-integer")),
        ],
    );
    let mut broken = Launch::new(&repair).spawn();
    assert_eq!(broken.exited(Duration::from_secs(15)), Some(0));
    assert!(
        !repair.join("room-state.json").exists(),
        "a partial import never becomes the room's state"
    );

    let db = rusqlite::Connection::open(repair.join("room-history.sqlite3")).unwrap();
    db.execute("DELETE FROM connectors WHERE id = 'bad-id'", [])
        .unwrap();
    drop(db);
    let mut repaired = Launch::new(&repair).start();
    assert_eq!(repaired.stop(), 0);
    assert_eq!(
        read_json(&repair.join("room-state.json"))["connectors"]["kept-id"]["host"],
        "repair-host"
    );
}
