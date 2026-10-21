//! A node's application state over a temporary private directory.

use std::sync::Arc;

use tempfile::TempDir;

use crate::control::devices::{DeviceRegistry, NodeIdentity};
use crate::control::room::Room;
use crate::server::AppState;
use crate::storage::PrivateDir;

/// A private directory that lives as long as the returned guard.
pub(in crate::server) fn private_dir() -> (TempDir, PrivateDir) {
    let temp = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(temp.path().join("core")).unwrap();
    (temp, dir)
}

pub(in crate::server) fn app_state(dir: &PrivateDir, room: Arc<Room>, host: &str) -> Arc<AppState> {
    let identity = NodeIdentity::load_or_create(dir).unwrap();
    let registry = DeviceRegistry::load(dir.clone()).unwrap();
    Arc::new(AppState::new(
        dir.clone(),
        identity,
        registry,
        "fixture".to_owned(),
        host.to_owned(),
        "127.0.0.1".to_owned(),
        10829,
        room,
    ))
}
