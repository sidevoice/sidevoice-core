//! Windows: Ctrl+C and Ctrl+Break of the console the core runs in, through `tokio`. A console-less core has neither:
//! it is asked to leave over its local channel.

use tokio::signal::windows::{ctrl_break, ctrl_c, CtrlBreak, CtrlC};

/// The listeners for Ctrl+C and Ctrl+Break.
pub struct Signals {
    interrupt: CtrlC,
    brk: CtrlBreak,
}

impl super::Stop for Signals {
    fn install() -> Signals {
        Signals {
            interrupt: ctrl_c().expect("Ctrl+C listener"),
            brk: ctrl_break().expect("Ctrl+Break listener"),
        }
    }

    async fn received(&mut self) {
        tokio::select! { _ = self.interrupt.recv() => (), _ = self.brk.recv() => () }
    }
}
