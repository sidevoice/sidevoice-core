//! Windows: no console and a group of its own for what is detached, a Job Object for a tree (`process-wrap`).

use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject, KillOnDrop};
use std::io;
use tokio::process::Command;
use windows_sys::Win32::Foundation::{
    SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
// `windows`, not `windows-sys`: `CreationFlags` takes its flags type.
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};

/// Rust starts every child with `bInheritHandles`, so a process detached from here would also inherit this one's own
/// standard handles: when this process was started with a pipe for its output (a desktop app, a test capturing it),
/// the reader would not see the pipe close until the detached process ended (the core's idle exit, minutes later).
/// They are made non-inheritable first; `Stdio::inherit` duplicates what it passes on, so no later spawn is
/// affected. Nothing maintained covers it (`std`'s handle list is unstable): own `unsafe`, three calls.
fn keep_own_stdio_out() -> io::Result<()> {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` returns this process's handle or none (skipped: nothing to close); only the inherit flag
        // of that handle changes.
        let failed = unsafe {
            let handle = GetStdHandle(which);
            !handle.is_null()
                && handle != INVALID_HANDLE_VALUE
                && SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) == 0
        };
        if failed {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// This OS's processes.
pub struct Os;

impl super::Process for Os {
    async fn spawn_detached(mut command: Command) -> io::Result<super::Child> {
        keep_own_stdio_out()?;
        command.kill_on_drop(false);
        let mut wrap = CommandWrap::from(command);
        // Breakaway is refused when an enclosing job does not permit it. Do not request it implicitly.
        wrap.wrap(CreationFlags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
        ));
        super::spawn(wrap, false).await
    }

    async fn spawn_tree(command: Command) -> io::Result<super::Child> {
        let mut wrap = CommandWrap::from(command);
        wrap.wrap(CreationFlags(CREATE_NO_WINDOW))
            .wrap(JobObject)
            .wrap(KillOnDrop);
        super::spawn(wrap, true).await
    }
}

// The Job Object is an owned handle, so reaping its leader does not invalidate tree identity.
pub(super) fn try_wait(child: &mut super::Child) -> io::Result<Option<std::process::ExitStatus>> {
    child.leader().try_wait()
}

pub(super) async fn wait(child: &mut super::Child) -> io::Result<std::process::ExitStatus> {
    child.leader().wait().await
}
