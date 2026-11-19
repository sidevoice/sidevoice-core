//! The run marker and the exit file: the two small private files this module reads and writes.
//!
//! - The marker (`run.json` in a program's own data directory): a random run id the caller makes (never sent anywhere), the
//!   version, when the run started, launchd's run count (macOS) and whether the panic hook recorded a panic of this run. Written
//!   at start, removed when the run ends by itself. One writer: the program itself.
//! - The exit file (`D/service/<job>.exit`): the crash type a manager's hook or the runner saw, and when. Written by
//!   `record-exit` (Linux) or the connector's runner (Windows), read and deleted by the job's next run.
//!
//! Both are read as untrusted: bounded, and only what has the shape of what is written is taken.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::classify::{is_crash_type, systemd_type};
use super::Job;
use crate::fs::{Files, Fs};

const MARKER: &str = "run.json";
const LIMIT: u64 = 4 * 1024;
/// How far before a run's start an exit file may be dated: clocks of two processes, not a different run.
const SLOP_SECS: u64 = 5;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What one run leaves in its own directory while it is alive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// A random id (hex), made by the caller: it only ties this run's files together and is never sent.
    pub run: String,
    pub version: String,
    /// Unix seconds.
    pub started: u64,
    /// launchd's `runs` for the job when this run started (macOS).
    #[serde(default)]
    pub launchd_runs: Option<u64>,
    /// The panic hook recorded a panic of this run: its record is richer than an exit status, so the exit is not reported.
    #[serde(default)]
    pub panicked: bool,
}

fn plain(text: &str, max: usize, allowed: impl Fn(char) -> bool) -> bool {
    !text.is_empty() && text.len() <= max && text.chars().all(allowed)
}

impl Marker {
    /// A marker for a run starting now.
    pub fn new(run: String, version: &str, launchd_runs: Option<u64>) -> Self {
        Self {
            run,
            version: version.to_owned(),
            started: now(),
            launchd_runs,
            panicked: false,
        }
    }

    fn valid(&self) -> bool {
        plain(&self.run, 64, |c| c.is_ascii_hexdigit() || c == '-')
            && plain(&self.version, 64, |c| {
                c.is_ascii_alphanumeric() || "._+-".contains(c)
            })
    }

    fn read(dir: &Path) -> Option<Self> {
        let bytes = Files::read_private(&dir.join(MARKER), LIMIT).ok()??;
        serde_json::from_slice::<Self>(&bytes)
            .ok()
            .filter(Self::valid)
    }

    /// Starts a run: returns the marker a previous run left (so it did not end by itself; `None` when there was none, or it is not
    /// ours) and writes `current` in its place.
    pub fn begin(dir: &Path, current: &Self) -> io::Result<Option<Self>> {
        Files::ensure_private_dir(dir)?;
        let previous = Self::read(dir);
        Files::write_json(&dir.join(MARKER), current)?;
        Ok(previous)
    }

    /// The run ended by itself (a return from `main`, a handled exit): no marker is left.
    pub fn end(dir: &Path) -> io::Result<()> {
        match std::fs::remove_file(dir.join(MARKER)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Says that a panic of run `run` was recorded: only if the marker is still that run's.
    pub fn panicked(dir: &Path, run: &str) -> io::Result<()> {
        match Self::read(dir) {
            Some(mut marker) if marker.run == run => {
                marker.panicked = true;
                Files::write_json(&dir.join(MARKER), &marker)
            }
            _ => Ok(()),
        }
    }
}

/// Where `job`'s exit file is, under `data` (`D`): `D/service/<name>.exit`.
pub fn exit_file(data: &Path, job: &Job) -> PathBuf {
    data.join("service").join(format!("{}.exit", job.name))
}

#[derive(Serialize, Deserialize)]
struct Exit {
    #[serde(rename = "type")]
    kind: String,
    /// Unix seconds.
    at: u64,
}

/// Writes `exception_type` as the job's last end, if it is a crash type; anything else is refused (`InvalidInput`).
pub fn record(path: &Path, exception_type: &str) -> io::Result<()> {
    if !is_crash_type(exception_type) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    if let Some(parent) = path.parent() {
        Files::ensure_private_dir(parent)?;
    }
    Files::write_json(
        path,
        &Exit {
            kind: exception_type.to_owned(),
            at: now(),
        },
    )
}

/// What `ExecStopPost=` knows, from the environment systemd sets (`get` reads a variable): writes the exit file when the end
/// was a crash. Returns whether it did. Never fails the unit's hook for a status that is not a crash.
pub fn record_systemd(path: &Path, get: impl Fn(&str) -> Option<String>) -> io::Result<bool> {
    let value = |name: &str| get(name).unwrap_or_default();
    match systemd_type(
        &value("SERVICE_RESULT"),
        &value("EXIT_CODE"),
        &value("EXIT_STATUS"),
    ) {
        Some(kind) => record(path, &kind).map(|()| true),
        None => Ok(false),
    }
}

/// Reads and deletes the exit file: the crash type, if the file is ours and was written at or after `since` (when the run that
/// ended started, unix seconds; a file from an older run is not about this one) and not in the future.
pub fn take(path: &Path, since: u64) -> Option<String> {
    let read = Files::read_private(path, LIMIT);
    let _ = std::fs::remove_file(path);
    let exit = serde_json::from_slice::<Exit>(&read.ok()??).ok()?;
    (is_crash_type(&exit.kind) && exit.at + SLOP_SECS >= since && exit.at <= now() + SLOP_SECS)
        .then_some(exit.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::{CONNECTOR, CORE};

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn an_exit_file_round_trips_once_and_only_for_the_run_it_ended() {
        let dir = tempfile::tempdir().unwrap();
        let path = exit_file(dir.path(), &CORE);
        assert!(path.ends_with("service/sidevoice-core.exit"));
        assert_ne!(path, exit_file(dir.path(), &CONNECTOR));
        record(&path, "ntstatus.0xC0000005").unwrap();
        assert_eq!(
            take(&path, now() - 1).as_deref(),
            Some("ntstatus.0xC0000005")
        );
        // Read once: it is gone.
        assert_eq!(take(&path, 0), None);
        // A file from before the run that ended is about an older run.
        record(&path, "abort.oom_kill").unwrap();
        assert_eq!(take(&path, now() + 3600), None);
        assert!(!path.exists());
    }

    #[test]
    fn what_is_not_a_crash_type_is_never_written_or_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = exit_file(dir.path(), &CORE);
        for text in ["abort.signal.SIGKILL", "panic", "/home/ana/secret", ""] {
            assert!(record(&path, text).is_err(), "{text}");
        }
        assert!(!path.exists());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"type":"/home/ana/secret","at":4102444800}"#).unwrap();
        assert_eq!(take(&path, 0), None);
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(take(&path, 0), None);
        assert!(!path.exists(), "a file that is not ours is deleted too");
    }

    #[test]
    fn the_systemd_hook_writes_only_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = exit_file(dir.path(), &CONNECTOR);
        let clean = env(&[
            ("SERVICE_RESULT", "success"),
            ("EXIT_CODE", "exited"),
            ("EXIT_STATUS", "0"),
        ]);
        assert!(!record_systemd(&path, clean).unwrap());
        assert!(!path.exists());
        let crash = env(&[
            ("SERVICE_RESULT", "core-dump"),
            ("EXIT_CODE", "dumped"),
            ("EXIT_STATUS", "SEGV"),
        ]);
        assert!(record_systemd(&path, crash).unwrap());
        assert_eq!(take(&path, 0).as_deref(), Some("abort.signal.SIGSEGV"));
        assert!(!record_systemd(&path, env(&[])).unwrap());
    }

    #[test]
    fn a_marker_left_behind_is_the_previous_run_and_a_clean_end_leaves_none() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("run");
        let first = Marker::new("a1b2c3".into(), "0.19.0", Some(3));
        assert_eq!(Marker::begin(&dir, &first).unwrap(), None);
        // The run dies: the next start finds the marker.
        let second = Marker::new("d4e5f6".into(), "0.20.0", Some(4));
        assert_eq!(Marker::begin(&dir, &second).unwrap().as_ref(), Some(&first));
        // A panic of a run that is not the marker's changes nothing; its own run's is kept.
        Marker::panicked(&dir, "a1b2c3").unwrap();
        assert!(!Marker::read(&dir).unwrap().panicked);
        Marker::panicked(&dir, "d4e5f6").unwrap();
        assert!(Marker::read(&dir).unwrap().panicked);
        // A clean end: nothing for the next start.
        Marker::end(&dir).unwrap();
        Marker::end(&dir).unwrap();
        let third = Marker::new("0a0b".into(), "0.20.0", None);
        assert_eq!(Marker::begin(&dir, &third).unwrap(), None);
    }

    #[test]
    fn a_marker_that_is_not_ours_is_no_marker() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("run");
        for text in [
            "not json".to_owned(),
            r#"{"run":"../../etc","version":"1","started":1}"#.to_owned(),
            format!(
                r#"{{"run":"ab","version":"{}","started":1}}"#,
                "x".repeat(100)
            ),
        ] {
            Files::ensure_private_dir(&dir).unwrap();
            std::fs::write(dir.join(MARKER), text).unwrap();
            let current = Marker::new("ab".into(), "1.0.0", None);
            assert_eq!(Marker::begin(&dir, &current).unwrap(), None);
        }
    }
}
