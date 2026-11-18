//! Process configuration, launch handshake, listeners and shutdown.

mod command;
mod config;
mod crash_reports;
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

pub use command::{Cli, Command, Switch};
pub use config::Config;
pub use crash_reports::crash_reports;
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
        service.name = "sidevoice-core",
        service.version = env!("CARGO_PKG_VERSION"),
        os.type = logging::os_type(std::env::consts::OS),
        os.name = logging::os_name(std::env::consts::OS),
        host.arch = logging::host_arch(std::env::consts::ARCH),
        "the core is starting"
    );
    match serve::serve(&config).await {
        Ok(()) => {
            info!(key = "runtime.log_stop", launch_id = %ClientId(&config.launch_id), "the core stopped");
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
