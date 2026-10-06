//! Why a launch failed, and the report a launcher reads when it does.

use serde_json::{json, Map, Value};

use super::event_log::{log_event, timestamp};
use super::{system_language, Config};
use crate::messages::{render, LocalizedMessage};
use crate::storage::PrivateDir;

pub(super) struct StartFailure {
    pub(super) step: &'static str,
    pub(super) key: &'static str,
    pub(super) status: i32,
    pub(super) params: Map<String, Value>,
}

impl StartFailure {
    pub(super) fn new(step: &'static str, key: &'static str) -> Self {
        Self {
            step,
            key,
            status: 0,
            params: Map::new(),
        }
    }

    /// Another Core already holds the directory lock.
    pub(super) fn running() -> Self {
        Self {
            status: 75,
            ..Self::new("bind", "bind.core-running")
        }
    }

    /// A listener stopped after the launch had been reported ready.
    pub(super) fn crashed() -> Self {
        Self {
            status: 1,
            ..Self::new("run", "start.failed")
        }
    }

    /// Whether the failure happened after the launch was reported ready, so it is not a start failure.
    pub(super) fn is_run(&self) -> bool {
        self.step == "run"
    }
}

/// Write `core-failure.json`, print the failure and log it; each part is best effort.
pub(super) fn report(config: &Config, error: &StartFailure) {
    let message = render(
        &LocalizedMessage {
            key: error.key.to_owned(),
            params: error.params.clone(),
        },
        &system_language(),
    );
    let report = json!({"launch_id": config.launch_id, "step": error.step, "key": error.key,
        "message": message, "at": timestamp()});
    if let Ok(dir) = PrivateDir::open_for_report(&config.data_dir) {
        let _ = dir.write_json("core-failure.json", &report);
    }
    eprintln!("{}", json!({"key": error.key, "message": message}));
    let _ = log_event(config, "runtime.log_failure", Some(error.key));
}
