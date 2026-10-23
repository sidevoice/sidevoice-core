//! Windows: another process's request to leave (this process's stop event, `sidevoice_platform::stop`), and Ctrl+C
//! and Ctrl+Break of the console the core runs in, through `tokio`.

use sidevoice_platform::stop::{StopRequest, StopRequests};
use tokio::signal::windows::{ctrl_break, ctrl_c, CtrlBreak, CtrlC};

/// The stop event's listener, and those for Ctrl+C and Ctrl+Break.
pub struct Signals {
    stop: StopRequests,
    interrupt: CtrlC,
    brk: CtrlBreak,
}

impl super::Stop for Signals {
    fn install() -> Signals {
        Signals {
            stop: StopRequests::listen().expect("stop event listener"),
            interrupt: ctrl_c().expect("Ctrl+C listener"),
            brk: ctrl_break().expect("Ctrl+Break listener"),
        }
    }

    async fn received(&mut self) {
        tokio::select! {
            _ = self.stop.received() => (),
            _ = self.interrupt.recv() => (),
            _ = self.brk.recv() => (),
        }
    }
}
