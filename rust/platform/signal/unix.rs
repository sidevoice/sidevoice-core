//! Unix: another process's request to leave (`SIGTERM`, `sidevoice_platform::stop`) and `SIGINT`, through `tokio`.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use sidevoice_platform::stop::{StopRequest, StopRequests};
use tokio::signal::unix::{signal, Signal, SignalKind};

/// The listeners for `SIGTERM` and `SIGINT`.
pub struct Signals {
    stop: StopRequests,
    interrupt: Signal,
}

impl super::Stop for Signals {
    fn install() -> Signals {
        Signals {
            stop: StopRequests::listen().expect("SIGTERM listener"),
            interrupt: signal(SignalKind::interrupt()).expect("SIGINT listener"),
        }
    }

    async fn received(&mut self) {
        tokio::select! { _ = self.stop.received() => (), _ = self.interrupt.recv() => () }
    }
}
