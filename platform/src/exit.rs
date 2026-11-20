//! How a program's last run ended, when it ended badly: the crash statuses the OS manager (or a runner) saw and the
//! program's own panic hook never could (a signal, an abort, an out-of-memory kill, a Windows exception).
//!
//! Decided with the operator (sidevoice-connector#366, option B): a program leaves a **run marker** in its own data
//! directory when it starts and removes it when it ends by itself ([`Marker`]); one still there at the next start means the
//! previous run did not end cleanly. What the manager saw is then read, **never from text a person could have written**,
//! only from numbers and fixed names, and only a crash status is kept:
//!
//! | Where | Who saw it | How it reaches the next run |
//! |---|---|---|
//! | Linux (systemd user unit) | `ExecStopPost=-<program> record-exit`, which gets `$SERVICE_RESULT`, `$EXIT_CODE` and `$EXIT_STATUS` ([`systemd_type`]) | an exit file ([`exit_file`], [`record`], [`take`]) |
//! | Windows (Task Scheduler) | the `run-job` runner, the child's real parent, which holds its whole exit code ([`ntstatus_type`]) | the same exit file |
//! | macOS (launchd) | launchd itself: `launchctl print gui/<uid>/<label>` ([`launchd`]), trusted only if its `runs` went up by exactly one since the marker | asked at the next start |
//!
//! Everything the classification returns is a fixed `exception.type` ([`is_crash_type`] is the closed list): `abort.signal.SIGSEGV`,
//! `abort.oom_kill`, `ntstatus.0xC0000005`. A `SIGKILL`, a `SIGTERM`, a normal exit code and any Windows code outside the
//! allowlist (a console close, a logoff) are a logout, a reboot, a force quit or a handled exit: not a crash, never returned.
//!
//! This is the one place that classifies and reads the exit file. Each program reads only its own job's facts ([`Job`]) and
//! sends only its own report; the writers are a program's `record-exit` subcommand and the connector's runner, the
//! reader is [`take`].

mod classify;
mod file;
pub mod launchd;

pub use classify::{is_crash_type, is_panic_abort, ntstatus_type, signal_type, systemd_type, Unix};
pub use file::{exit_file, record, record_systemd, take, Marker};

/// One of the jobs a manager runs, as the manager knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job {
    /// The launchd label (macOS).
    pub label: &'static str,
    /// The unit and task name, and the exit file's stem.
    pub name: &'static str,
}

/// The core's job.
pub const CORE: Job = Job {
    label: "ai.sidevoice.core",
    name: "sidevoice-core",
};
/// The connector's job.
pub const CONNECTOR: Job = Job {
    label: "ai.sidevoice.connector",
    name: "sidevoice-connector",
};
