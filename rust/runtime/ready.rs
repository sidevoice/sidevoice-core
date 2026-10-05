//! The ready file: the launch handshake a launcher reads to find and authenticate this Core.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::failure::StartFailure;
use super::{Config, API, CONNECTOR_PROTOCOL};
use crate::storage::PrivateDir;

/// What a launcher needs to reach this process and connect to its Room.
pub(super) fn document(config: &Config, port: u16, connector_id: &str, token: &str) -> Value {
    json!({"pid": std::process::id(), "port": port, "url": format!("http://127.0.0.1:{port}"),
        "socket": config.socket, "launch_id": config.launch_id, "version": env!("CARGO_PKG_VERSION"),
        "api": API, "protocol": CONNECTOR_PROTOCOL, "connector_protocols": [CONNECTOR_PROTOCOL, 3],
        "connector_id": connector_id, "token": token})
}

/// Publish the ready file privately, next to the data directory unless configured elsewhere.
pub(super) fn write(config: &Config, data_dir: &Path, ready: &Value) -> Result<(), StartFailure> {
    let ready_dir = PrivateDir::open(config.ready_file.parent().unwrap_or(data_dir))
        .map_err(|_| StartFailure::new("start", "start.failed"))?;
    let name = config
        .ready_file
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("core.json");
    ready_dir
        .write_json(name, ready)
        .map_err(|_| StartFailure::new("start", "start.failed"))
}

/// Remove the ready file only if it still describes this process.
pub(super) fn remove_own(path: &Path) {
    if fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("pid").and_then(Value::as_u64))
        == Some(std::process::id() as u64)
    {
        let _ = fs::remove_file(path);
    }
}
