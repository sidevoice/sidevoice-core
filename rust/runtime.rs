//! Process configuration, launch handshake, listeners and shutdown.

mod command;
mod config;
mod directories;
mod event_log;
mod failure;
mod language;
mod local_socket;
mod ready;
mod self_test;
mod serve;
mod servers;
mod stop;

#[cfg(test)]
mod tests;

pub use command::{Command, CommandError};
pub use config::Config;
pub use language::system_language;
pub use self_test::self_test;

use failure::StartFailure;

pub const API: u8 = 1;
const CONNECTOR_PROTOCOL: u8 = 2;

/// Serve until stopped and return the process exit status.
pub async fn run(config: Config) -> i32 {
    if event_log::log_event(&config, "runtime.log_start", None).is_err() {
        failure::report(&config, &StartFailure::new("start", "start.failed"));
        return 0;
    }
    match serve::serve(&config).await {
        Ok(()) => {
            let _ = event_log::log_event(&config, "runtime.log_stop", None);
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
