//! What ends a running process: a termination signal or a quiet period without calls.

use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{signal, SignalKind};

use crate::server::AppState;

/// SIGTERM and SIGINT listeners. They are installed before the core says it is ready, so a
/// signal a manager sends once it sees the ready file is held, never lost to the default action.
pub(super) struct Signals {
    term: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

impl Signals {
    pub(super) fn install() -> Self {
        Self {
            term: signal(SignalKind::terminate()).expect("SIGTERM listener"),
            interrupt: signal(SignalKind::interrupt()).expect("SIGINT listener"),
        }
    }

    /// Resolve on SIGTERM or SIGINT.
    pub(super) async fn received(&mut self) {
        tokio::select! { _ = self.term.recv() => (), _ = self.interrupt.recv() => () }
    }
}

/// Resolve once no call has been open and no connector linked for `seconds`.
pub(super) async fn idle_for(state: Arc<AppState>, seconds: f64) {
    let quiet = Duration::from_secs_f64(seconds.max(0.01));
    let mut since = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs_f64(seconds.clamp(0.01, 5.0))).await;
        // A linked connector keeps the core up as much as an open call does.
        if state.open_calls() > 0 || state.room.has_connector() {
            since = tokio::time::Instant::now();
        } else if since.elapsed() >= quiet {
            return;
        }
    }
}
