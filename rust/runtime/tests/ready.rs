use std::fs;

use serde_json::json;

use super::support::full_config;
use crate::runtime::ready::{document, remove_own};

#[test]
fn document_describes_this_process_and_its_protocols() {
    let ready = document(&full_config(), 9000, "connector", "token");
    assert_eq!(
        ready,
        json!({"pid": std::process::id(), "port": 9000, "url": "http://127.0.0.1:9000",
            "socket": "/run/sidevoice/local.sock", "launch_id": "launch",
            "version": env!("CARGO_PKG_VERSION"), "api": 1, "protocol": 2,
            "connector_protocols": [2, 3], "connector_id": "connector", "token": "token"})
    );
}

#[test]
fn only_this_process_ready_file_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let own = dir.path().join("own.json");
    let other = dir.path().join("other.json");
    fs::write(&own, json!({"pid": std::process::id()}).to_string()).unwrap();
    fs::write(
        &other,
        json!({"pid": u64::from(std::process::id()) + 1}).to_string(),
    )
    .unwrap();
    remove_own(&own);
    remove_own(&other);
    assert!(!own.exists());
    assert!(other.exists());
}
