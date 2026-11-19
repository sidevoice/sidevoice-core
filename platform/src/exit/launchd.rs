//! macOS: what launchd remembers of a job's last run, from `launchctl print gui/<uid>/<label>`.
//!
//! launchd has no stop hook, so the new run asks. The text of `launchctl print` is not an API (its man page says so; the
//! connector's service status already depends on the same lines), so only numbers are read: `runs`, `last exit code` and the
//! number after the colon of `last terminating signal` (`Segmentation fault: 11`). The words are never matched.

use super::classify::{signal_type, Unix};

/// The lines of the job's own block that matter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// How many times launchd has started the job.
    pub runs: Option<u64>,
    /// `None` for `(never exited)` and for a missing line.
    pub last_exit_code: Option<i64>,
    pub last_signal: Option<i32>,
}

/// Reads `launchctl print`'s text. The first value of each key wins: they are the job's own, before its nested blocks.
pub fn parse(text: &str) -> State {
    let mut state = State::default();
    let mut seen = [false; 3];
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "runs" if !seen[0] => {
                seen[0] = true;
                state.runs = value.parse().ok();
            }
            "last exit code" if !seen[1] => {
                seen[1] = true;
                state.last_exit_code = value.parse().ok();
            }
            "last terminating signal" if !seen[2] => {
                seen[2] = true;
                state.last_signal = value
                    .rsplit(':')
                    .next()
                    .and_then(|number| number.trim().parse().ok());
            }
            _ => {}
        }
    }
    state
}

/// The type of the previous run's end, trusted only when launchd started the job exactly once since the marker (`before` is the
/// `runs` the marker kept): any other count means the numbers belong to some other run, or the marker is from before launchd
/// counted. Only a crash signal is a crash.
pub fn crash_type(before: Option<u64>, now: &State) -> Option<String> {
    let (before, runs) = (before?, now.runs?);
    if runs != before.checked_add(1)? {
        return None;
    }
    signal_type(now.last_signal?, Unix::Macos)
}

/// How long `launchctl print` may take: launchd can stall, and nothing here is worth waiting for.
pub const PRINT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Runs `command` and returns its standard output if it succeeded within `timeout`; a stalled command is killed.
pub fn output_within(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<String> {
    use std::io::Read;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().ok_or(std::io::ErrorKind::BrokenPipe)?;
    // Read on a thread so a full pipe never blocks the child, and the wait below can give up.
    let (done, text) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.by_ref().take(1024 * 1024).read_to_end(&mut bytes);
        let _ = done.send(bytes);
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    if !status.success() {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    let bytes = text
        .recv_timeout(std::time::Duration::from_secs(1))
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `launchctl print gui/<uid>/<label>` (this user's uid)'s output, within [`PRINT_TIMEOUT`]; an error where launchd cannot be asked
/// (not macOS, no such job, a stall).
#[cfg(target_os = "macos")]
pub fn print(label: &str) -> std::io::Result<String> {
    let mut command = std::process::Command::new("/bin/launchctl");
    command.arg("print").arg(format!(
        "gui/{}/{label}",
        rustix::process::geteuid().as_raw()
    ));
    output_within(command, PRINT_TIMEOUT)
}

/// `launchctl print gui/<uid>/<label>` (this user's uid)'s output; an error where launchd cannot be asked (not macOS, no such job).
#[cfg(not(target_os = "macos"))]
pub fn print(_label: &str) -> std::io::Result<String> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRASHED: &str = "gui/501/ai.sidevoice.core = {\n\tactive count = 1\n\tstate = running\n\n\tprogram = /Users/ana/x/sidevoice-core\n\tdefault environment = {\n\t\tPATH => /usr/bin\n\t}\n\truns = 4\n\tpid = 4242\n\timmediate reason = inefficient\n\tlast exit code = (never exited)\n\tlast terminating signal = Segmentation fault: 11\n\tproperties = keepalive | runatload\n}\n";

    #[test]
    fn a_crash_is_read_from_the_numbers_only() {
        let state = parse(CRASHED);
        assert_eq!(
            state,
            State {
                runs: Some(4),
                last_exit_code: None,
                last_signal: Some(11),
            }
        );
        assert_eq!(
            crash_type(Some(3), &state).as_deref(),
            Some("abort.signal.SIGSEGV")
        );
    }

    #[test]
    fn the_status_is_trusted_only_when_runs_went_up_by_one() {
        let state = parse(CRASHED);
        assert_eq!(crash_type(Some(4), &state), None);
        assert_eq!(crash_type(Some(2), &state), None);
        assert_eq!(crash_type(Some(9), &state), None);
        assert_eq!(crash_type(None, &state), None);
        assert_eq!(crash_type(Some(u64::MAX), &state), None);
    }

    #[test]
    fn exits_kills_and_noise_are_not_crashes() {
        let kill = "\truns = 2\n\tlast exit code = (never exited)\n\tlast terminating signal = Killed: 9\n";
        assert_eq!(crash_type(Some(1), &parse(kill)), None);
        let term = "\truns = 2\n\tlast terminating signal = Terminated: 15\n";
        assert_eq!(crash_type(Some(1), &parse(term)), None);
        let exited = "\truns = 2\n\tlast exit code = 1\n";
        assert_eq!(parse(exited).last_exit_code, Some(1));
        assert_eq!(crash_type(Some(1), &parse(exited)), None);
        assert_eq!(parse(""), State::default());
        assert_eq!(parse("garbage = = =\nruns = many"), State::default());
        // The text never decides: a name with another number is read by its number.
        let bus = "\truns = 2\n\tlast terminating signal = Segmentation fault: 10\n";
        assert_eq!(
            crash_type(Some(1), &parse(bus)).as_deref(),
            Some("abort.signal.SIGBUS")
        );
    }
}

#[cfg(all(test, unix))]
mod timeout_tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn output_is_returned_and_a_stalled_command_is_killed_at_the_timeout() {
        let text = output_within(sh("echo runs = 4"), Duration::from_secs(5)).unwrap();
        assert_eq!(text.trim(), "runs = 4");
        assert!(output_within(sh("exit 3"), Duration::from_secs(5)).is_err());
        let started = Instant::now();
        let error = output_within(sh("exec sleep 30"), Duration::from_millis(200)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
