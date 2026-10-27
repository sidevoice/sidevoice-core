//! Unix child groups, through process-wrap.
use process_wrap::tokio::{CommandWrap, KillOnDrop, ProcessGroup};
use std::io;
use tokio::process::Command;

/// This OS's child execution.
pub struct Os;

impl super::Process for Os {
    async fn spawn_tree(command: Command) -> io::Result<super::Child> {
        let mut wrap = group(command);
        wrap.wrap(KillOnDrop);
        super::spawn(wrap, true).await
    }

    async fn spawn_detached(mut command: Command) -> io::Result<super::Child> {
        command.kill_on_drop(false);
        super::spawn(group(command), false).await
    }
}

fn group(command: Command) -> CommandWrap {
    let mut wrap = CommandWrap::from(command);
    wrap.wrap(ProcessGroup::leader());
    wrap
}

// Observe exit without releasing the PID: as long as the leader remains waitable, its numeric PID/PGID
// cannot name a new group. A probe for group existence after reaping would still have a TOCTOU race.
pub(super) fn try_wait(child: &mut super::Child) -> io::Result<Option<std::process::ExitStatus>> {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};
    use std::os::unix::process::ExitStatusExt;

    if !child.signal_armed {
        return child.leader().try_wait();
    }
    let pid = child
        .id()
        .and_then(|pid| Pid::from_raw(pid as i32))
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    let Some(status) = waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    )?
    else {
        return Ok(None);
    };
    // Linux/macOS wait status encoding, preserving core-dump status.
    let raw = match status.exit_status() {
        Some(code) => code << 8,
        None => {
            status
                .terminating_signal()
                .expect("EXITED selects exit or signal")
                | if status.dumped() { 0x80 } else { 0 }
        }
    };
    Ok(Some(std::process::ExitStatus::from_raw(raw)))
}

pub(super) async fn wait(child: &mut super::Child) -> io::Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = try_wait(child)? {
            return Ok(status);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Whether the leader `pid` (a child of this process) has been reaped, not merely killed: a zombie still answers
/// `waitid`, a reaped child is `ECHILD`.
#[cfg(test)]
pub(super) fn leader_reaped(pid: u32) -> bool {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    let pid = Pid::from_raw(pid as i32).unwrap();
    waitid(WaitId::Pid(pid), options) == Err(rustix::io::Errno::CHILD)
}
