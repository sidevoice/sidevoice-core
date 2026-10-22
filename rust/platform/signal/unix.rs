//! Unix: `SIGTERM` and `SIGINT`, through `tokio`.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use tokio::signal::unix::{signal, Signal, SignalKind};

/// The listeners for `SIGTERM` and `SIGINT`.
pub struct Signals {
    term: Signal,
    interrupt: Signal,
}

impl super::Stop for Signals {
    fn install() -> Signals {
        Signals {
            term: signal(SignalKind::terminate()).expect("SIGTERM listener"),
            interrupt: signal(SignalKind::interrupt()).expect("SIGINT listener"),
        }
    }

    async fn received(&mut self) {
        tokio::select! { _ = self.term.recv() => (), _ = self.interrupt.recv() => () }
    }
}
