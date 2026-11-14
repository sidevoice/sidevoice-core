//! Why a launch failed, and the report a launcher reads when it does.

use serde_json::{json, Map, Value};

use tracing::error;

use super::{system_language, Config};
use crate::log_safe::ClientId;
use crate::messages::{render, LocalizedMessage};
use crate::storage::ReportDir;

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

/// The current UTC time as RFC 3339 with milliseconds.
fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Write `core-failure.json`, print the failure and log it; each part is best effort.
// The launcher reads the stderr line (`core.stderr.log`).
#[allow(
    clippy::disallowed_macros,
    reason = "the launcher's report is a stderr line"
)]
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
    if let Ok(dir) = ReportDir::open(&config.data_dir) {
        let _ = dir.write_json("core-failure.json", &report);
    }
    eprintln!("{}", json!({"key": error.key, "message": message}));
    error!(
        key = "runtime.log_failure",
        code = error.key,
        step = error.step,
        launch_id = %ClientId(&config.launch_id),
        "core start failed"
    );
}
