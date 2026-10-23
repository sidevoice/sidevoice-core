//! What the operating system sends a process to ask it to leave, held from the moment it is installed.
//!
//! The interface is the [`Stop`] trait; [`Signals`] is the one OS implementation compiled in, chosen below. On every
//! OS, another process's request that this one leave, as `sidevoice_platform::stop` hears it (the connector asks that
//! way): Unix `SIGTERM` (what launchd and systemd send too), Windows the core's own stop event, which reaches a
//! console-less core. Besides it, the console's interrupts: Unix `SIGINT`; Windows Ctrl+C and Ctrl+Break, when the
//! core runs in a console. Callers import both: `use crate::platform::signal::{Signals, Stop};` then `Signals::install()`,
//! `signals.received().await`.

#[cfg(unix)]
#[path = "signal/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "signal/windows.rs"]
mod imp;

pub use imp::Signals;

use std::future::Future;

/// What every OS implements ([`Signals`], in `signal/unix.rs` and `signal/windows.rs`).
pub trait Stop: Sized {
    /// Starts listening. From here on such a request is held until [`Stop::received`] takes it, never lost to the
    /// default action (which ends the process at once): install it before telling anyone the process is ready.
    fn install() -> Self;

    /// Resolves once the process has been asked to leave.
    fn received(&mut self) -> impl Future<Output = ()> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nothing_asked_is_nothing_received() {
        let mut signals = Signals::install();
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), signals.received()).await;
        assert!(waited.is_err());
    }
}
