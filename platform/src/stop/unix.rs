//! Unix: `SIGTERM`, heard through `tokio` and sent with `rustix`.

use rustix::io::Errno;
use rustix::process::{kill_process, Pid, Signal};
use std::io;
use tokio::signal::unix::{signal, SignalKind};

/// The `SIGTERM` listener.
pub struct Os(tokio::signal::unix::Signal);

impl super::StopRequest for Os {
    fn listen() -> io::Result<Os> {
        signal(SignalKind::terminate()).map(Os)
    }

    async fn received(&mut self) {
        self.0.recv().await;
    }

    fn ask(pid: u32) -> io::Result<bool> {
        let pid = Pid::from_raw(super::checked(pid)?).expect("a checked pid is positive");
        match kill_process(pid, Signal::TERM) {
            Ok(()) => Ok(true),
            Err(Errno::SRCH) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}
