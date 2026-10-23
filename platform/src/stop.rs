//! Asking another of this user's processes to leave, and hearing that request: the one way to stop a process
//! gracefully that is the same, from the caller's side, on every OS. Whoever stops a process asks it to leave
//! ([`StopRequest::ask`]), gives it a grace time and only then kills it; a process that wants to leave in order
//! listens ([`StopRequest::listen`]) from startup.
//!
//! The interface is the [`StopRequest`] trait; [`StopRequests`] is the one OS implementation compiled in, chosen
//! below. Unix: `SIGTERM`, the standard (launchd and systemd send it too). Windows has no signal another process can
//! send to one without a console, so the request is a named event, `Local\sidevoice-stop-<pid>`, that the process
//! creates for itself and waits on, and that the asker sets (decision "Graceful stop", 2026-10-23, option C). The
//! name and both ends of it live here only, so the core, the connector and whoever stops them agree on it. Callers
//! import both: `use sidevoice_platform::stop::{StopRequest, StopRequests};` then `StopRequests::listen()?` and
//! `requests.received().await`, or `StopRequests::ask(pid)?`.
//!
//! Asking is by pid alone: a pid is only a hint (pids are reused), so a caller makes sure `pid` is still the process
//! it means before asking.

#[cfg(unix)]
#[path = "stop/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "stop/windows.rs"]
mod imp;

pub use imp::Os as StopRequests;

use std::future::Future;
use std::io;

/// What every OS implements ([`StopRequests`], in `stop/unix.rs` and `stop/windows.rs`).
pub trait StopRequest: Sized {
    /// Starts listening for a request that this process leave. From here on a request is held until
    /// [`StopRequest::received`] takes it, never lost, and the OS's default action for it (Unix: the process ends at
    /// once) no longer applies: listen before telling anyone the process is ready. Process-wide: every listener in
    /// the process hears every request. Needs a `tokio` runtime (Unix).
    fn listen() -> io::Result<Self>;

    /// Resolves once this process has been asked to leave. Meant to be awaited until it resolves once.
    fn received(&mut self) -> impl Future<Output = ()> + Send;

    /// Asks the process `pid` to leave. `false` when there is no such process or it does not listen (Windows: it
    /// made no event), so nothing was asked and only a kill will stop it. A pid that can name no single process
    /// (0, 1, beyond `i32`) is `InvalidInput`.
    fn ask(pid: u32) -> io::Result<bool>;
}

/// `pid` as a process the OSes can be asked about: 0 and 1 are never ours (the idle or init process, and "every
/// process" to Unix `kill`), and a negative `pid_t` names a group.
fn checked(pid: u32) -> io::Result<i32> {
    i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 1)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{pid} names no single process"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // The only test that asks: a request is process-wide, and any other listener in this test binary would hear it.
    #[tokio::test]
    async fn a_listener_waits_until_asked_and_hears_the_request() {
        let mut requests = StopRequests::listen().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), requests.received())
                .await
                .is_err()
        );
        assert!(StopRequests::ask(std::process::id()).unwrap());
        tokio::time::timeout(Duration::from_secs(5), requests.received())
            .await
            .expect("the request was lost");
    }

    #[test]
    fn a_pid_that_names_no_process_is_not_asked() {
        for pid in [0, 1, u32::MAX] {
            let error = StopRequests::ask(pid).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{pid}");
        }
        // Beyond any OS's pid range (Linux `pid_max`, macOS 99 999) and odd (Windows pids are multiples of 4).
        assert!(!StopRequests::ask(i32::MAX as u32).unwrap());
    }
}
