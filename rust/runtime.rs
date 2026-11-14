//! Process configuration, launch handshake, listeners and shutdown.

mod command;
mod config;
mod directories;
mod event_log;
mod failure;
mod language;
mod limited_listener;
mod logging;
mod ready;
mod serve;
mod servers;
mod stop;

#[cfg(test)]
mod tests;

pub use command::Cli;
pub use config::Config;
pub use language::system_language;

use failure::StartFailure;
use tracing::info;

use crate::log_safe::ClientId;

pub const API: u8 = 1;
const CONNECTOR_PROTOCOL: u8 = 2;

/// Install the process log, the first thing a launch does. A log that cannot be opened is a start failure, reported
/// here; the error is the exit status.
pub fn init_logging(config: &Config) -> Result<(), i32> {
    logging::init(config).map_err(|_| {
        failure::report(config, &StartFailure::new("start", "start.failed"));
        0
    })
}

/// Serve until stopped and return the process exit status.
pub async fn run(config: Config) -> i32 {
    info!(
        key = "runtime.log_start",
        launch_id = %ClientId(&config.launch_id),
        version = env!("CARGO_PKG_VERSION"),
        "core starting"
    );
    match serve::serve(&config).await {
        Ok(()) => {
            info!(key = "runtime.log_stop", launch_id = %ClientId(&config.launch_id), "core stopped");
            0
        }
        Err(error) => {
            if !error.is_run() {
                failure::report(&config, &error);
            }
            error.status
        }
    }
}
