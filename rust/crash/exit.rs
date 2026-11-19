//! Abnormal exits: what the panic hook never sees (a signal, an abort, an out-of-memory kill, a Windows exception).
//!
//! Decided with the operator (sidevoice-connector#366, option B). At start the core leaves a run marker in its data directory
//! (`sidevoice_platform::exit::Marker`: a random run id that is never sent) and removes it when `run` returns by itself
//! ([`finish`]). A marker still there at the next start means the previous run did not end cleanly; the crash status the OS
//! manager saw is then read ([`begin`]; the classification and the exit file are `sidevoice_platform::exit`'s, once for
//! the core and the connector) and, only if it is a crash status, one record is made ([`record`]) with the persisted tail
//! of that run (`tail`). A panic of the same run suppresses it (the hook marks the marker, `crash::write`): the panic record
//! is richer. A `SIGKILL`, a force quit and any other status are never reported.
//!
//! Each signature (version, type) is made at most once a day on this machine, with no count (`Cap`): a manager that
//! restarts a crashing core every ten seconds sends one report, not thousands. The same switch, closed field list, store and
//! sender as the panics' apply: nothing is made while the switch is off.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::PoisonError;
use std::time::{SystemTime, UNIX_EPOCH};

use sidevoice_platform::exit::{self as platform_exit, launchd, Marker};

use super::record::{Abnormal, CrashRecord};
use super::{settings, tail, Context};
use crate::platform::data_dir;
use crate::platform::fs::{Files, Fs};

const CAP: &str = "crash-exit-cap.json";
const LIMIT: u64 = 8 * 1024;
const DAY_SECS: u64 = 24 * 60 * 60;
/// The most signatures the cap remembers.
const MAX_SIGNATURES: usize = 32;

/// The previous run, when it ended in a crash status worth a record.
pub(super) struct Previous {
    pub(super) version: String,
    pub(super) kind: String,
    /// The tail the previous run mirrored, rendered.
    pub(super) tail: Vec<String>,
}

/// Where the core's exit file is: under `D`, as the connector's service layer writes it.
fn exit_path() -> Option<std::path::PathBuf> {
    let data = data_dir::resolve(|name| std::env::var_os(name)).ok()?;
    Some(platform_exit::exit_file(&data, &platform_exit::CORE))
}

/// Starts this run: reads what the previous one left (its tail, before this run writes its own) and puts this run's marker.
/// Best effort: nothing here fails the core.
pub(super) fn begin(data: &Path, run: &str, version: &str) -> Option<Previous> {
    let state = launchd::print(platform_exit::CORE.label)
        .ok()
        .map(|text| launchd::parse(&text));
    begin_with(data, run, version, state, exit_path())
}

/// [`begin`] with what it asks the OS for given: launchd's state and the exit file's place.
fn begin_with(
    data: &Path,
    run: &str,
    version: &str,
    state: Option<launchd::State>,
    exit_file: Option<std::path::PathBuf>,
) -> Option<Previous> {
    let current = Marker::new(run.to_owned(), version, state.as_ref().and_then(|s| s.runs));
    let Some(marker) = Marker::begin(data, &current).ok().flatten() else {
        // A clean end or a first start: an exit file left by a crash status is about no run of ours.
        if let Some(path) = &exit_file {
            let _ = std::fs::remove_file(path);
        }
        return None;
    };
    // The exit file is read (and deleted) whatever the marker says, so that none is left for a later run.
    let seen = exit_file.and_then(|path| platform_exit::take(&path, marker.started));
    if marker.panicked {
        return None;
    }
    let kind = seen.or_else(|| {
        state
            .as_ref()
            .and_then(|state| launchd::crash_type(marker.launchd_runs, state))
    })?;
    Some(Previous {
        version: marker.version,
        kind,
        tail: tail::render_previous(&tail::previous(data)),
    })
}

/// The run ended by itself: no marker is left for the next start.
pub(super) fn finish(data: &Path) {
    let _ = Marker::end(data);
}

/// Makes the record of the previous run's exit, once the switch and the day's cap allow. Called by the sender's thread.
pub(super) fn record(context: &Context) {
    let previous = context
        .previous
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let Some(previous) = previous else { return };
    if !settings::enabled(&context.data) || !permit(&context.data, &previous, unix_now()) {
        return;
    }
    let record = CrashRecord::from_abnormal(
        Abnormal {
            version: &previous.version,
            os: context.os,
            os_name: context.os_name,
            arch: context.arch,
            at: SystemTime::now().into(),
            kind: &previous.kind,
            tail: previous.tail,
        },
        &context.homes,
    );
    let _ = context.store.record(record);
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Whether the (version, type) of `previous` may be made now, and remembers that it was. A clock earlier than the last one counts
/// as a new day.
fn permit(data: &Path, previous: &Previous, now: u64) -> bool {
    let key = format!("{}|{}", previous.version, previous.kind);
    let mut sent: BTreeMap<String, u64> = Files::read_private(&data.join(CAP), LIMIT)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if sent
        .get(&key)
        .is_some_and(|last| now >= *last && now - last < DAY_SECS)
    {
        return false;
    }
    sent.retain(|_, last| now < *last || now - *last < 2 * DAY_SECS);
    sent.insert(key, now);
    while sent.len() > MAX_SIGNATURES {
        let Some(oldest) = sent
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        sent.remove(&oldest);
    }
    let _ = Files::write_json(&data.join(CAP), &sent);
    true
}

/// Deletes the cap.
pub(super) fn remove_cap(data: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(data.join(CAP)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn previous(version: &str, kind: &str) -> Previous {
        Previous {
            version: version.to_owned(),
            kind: kind.to_owned(),
            tail: Vec::new(),
        }
    }

    fn private(temp: &tempfile::TempDir) -> std::path::PathBuf {
        let dir = temp.path().join("core");
        Files::ensure_private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn one_record_per_signature_per_day() {
        let temp = tempfile::tempdir().unwrap();
        let dir = private(&temp);
        let segv = previous("0.20.0", "abort.signal.SIGSEGV");
        assert!(permit(&dir, &segv, 1_000_000));
        assert!(!permit(&dir, &segv, 1_000_010), "a crash loop sends one");
        assert!(!permit(&dir, &segv, 1_000_000 + DAY_SECS - 1));
        // Another type, another version, and the next day are other signatures.
        assert!(permit(
            &dir,
            &previous("0.20.0", "abort.signal.SIGABRT"),
            1_000_020
        ));
        assert!(permit(
            &dir,
            &previous("0.21.0", "abort.signal.SIGSEGV"),
            1_000_030
        ));
        assert!(permit(&dir, &segv, 1_000_000 + DAY_SECS));
        // A clock that went back is a new day.
        assert!(permit(&dir, &segv, 500));
    }

    #[test]
    fn the_cap_is_bounded_and_a_foreign_file_is_no_cap() {
        let temp = tempfile::tempdir().unwrap();
        let dir = private(&temp);
        for n in 0..50 {
            assert!(permit(
                &dir,
                &previous(&format!("1.{n}.0"), "abort.oom_kill"),
                1_000 + n
            ));
        }
        let kept: BTreeMap<String, u64> =
            serde_json::from_slice(&std::fs::read(dir.join(CAP)).unwrap()).unwrap();
        assert_eq!(kept.len(), MAX_SIGNATURES);
        std::fs::write(dir.join(CAP), b"[1,2").unwrap();
        assert!(permit(&dir, &previous("1.0.0", "abort.oom_kill"), 5_000));
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::crash::{tail, TAIL_LINES};
    use std::path::PathBuf;

    struct Dir {
        _temp: tempfile::TempDir,
        data: PathBuf,
        exit: PathBuf,
    }

    fn dir() -> Dir {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("core");
        Files::ensure_private_dir(&data).unwrap();
        let exit = platform_exit::exit_file(temp.path(), &platform_exit::CORE);
        Dir {
            _temp: temp,
            data,
            exit,
        }
    }

    fn start(d: &Dir, run: &str, state: Option<launchd::State>) -> Option<Previous> {
        begin_with(&d.data, run, "0.20.0", state, Some(d.exit.clone()))
    }

    #[test]
    fn a_crash_status_after_a_run_that_did_not_end_makes_one_previous() {
        let d = dir();
        assert!(
            start(&d, "aa", None).is_none(),
            "the first start has no previous"
        );
        platform_exit::record(&d.exit, "ntstatus.0xC0000005").unwrap();
        let previous = start(&d, "bb", None).expect("a crash");
        assert_eq!(previous.kind, "ntstatus.0xC0000005");
        assert_eq!(previous.version, "0.20.0");
        // The exit file was read once.
        assert!(!d.exit.exists());
    }

    #[test]
    fn a_run_that_ended_by_itself_or_died_without_a_crash_status_reports_nothing() {
        let d = dir();
        start(&d, "aa", None);
        finish(&d.data);
        platform_exit::record(&d.exit, "abort.signal.SIGSEGV").unwrap();
        assert!(start(&d, "bb", None).is_none(), "no marker: a clean end");
        // Killed (no crash status): the marker is left, nothing was recorded.
        assert!(start(&d, "cc", None).is_none());
    }

    #[test]
    fn a_panic_of_the_same_run_suppresses_the_exit() {
        let d = dir();
        start(&d, "aa", None);
        Marker::panicked(&d.data, "aa").unwrap();
        platform_exit::record(&d.exit, "abort.signal.SIGABRT").unwrap();
        assert!(start(&d, "bb", None).is_none());
        assert!(!d.exit.exists(), "its exit file is gone all the same");
        // The next run's own exit is reported again.
        platform_exit::record(&d.exit, "abort.signal.SIGABRT").unwrap();
        assert!(start(&d, "cc", None).is_some());
    }

    #[test]
    fn launchd_is_asked_only_when_its_run_count_went_up_by_one() {
        let d = dir();
        let state = |runs| launchd::State {
            runs: Some(runs),
            last_exit_code: None,
            last_signal: Some(11),
        };
        start(&d, "aa", Some(state(7)));
        let previous = start(&d, "bb", Some(state(8))).expect("one more run");
        assert_eq!(previous.kind, "abort.signal.SIGSEGV");
        assert!(
            start(&d, "cc", Some(state(12))).is_none(),
            "several runs: not ours"
        );
    }

    #[test]
    fn the_record_has_a_fixed_message_no_frames_and_no_personal_data() {
        let d = dir();
        let context = Context {
            data: d.data.clone(),
            store: crate::crash::CrashStore::new(&d.data, vec!["/home/ana".to_owned()]),
            homes: vec!["/home/ana".to_owned()],
            os: "linux",
            os_name: "Linux",
            arch: "amd64",
            images: Vec::new(),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            run: "ab".to_owned(),
            previous: std::sync::Mutex::new(Some(Previous {
                version: "0.20.0".to_owned(),
                kind: "abort.signal.SIGSEGV".to_owned(),
                tail: vec!["+0.000s WARN sidevoice_core::x: failed at /home/ana/secret".to_owned()],
            })),
        };
        record(&context);
        record(&context);
        let records = context.store.pending().unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.exception.kind, "abort.signal.SIGSEGV");
        assert_eq!(record.exception.message, "the process ended abnormally");
        assert!(record.backtrace.is_empty() && record.images.is_empty());
        let text = serde_json::to_string(&crate::crash::event(record, "m")).unwrap();
        assert!(!text.contains("ana"), "{text}");
        assert!(text.contains("~/secret"), "{text}");
        // The day's cap holds a second one back, whatever restarts did.
        *context.previous.lock().unwrap() = Some(previous_of("abort.signal.SIGSEGV"));
        record_again(&context);
        assert_eq!(context.store.pending().unwrap().len(), 1);
        assert_eq!(context.store.pending().unwrap()[0].count, 1);
    }

    fn previous_of(kind: &str) -> Previous {
        Previous {
            version: "0.20.0".to_owned(),
            kind: kind.to_owned(),
            tail: Vec::new(),
        }
    }

    fn record_again(context: &Context) {
        record(context);
    }

    #[test]
    fn the_mirror_is_bounded_re_keyed_and_survives_a_restart() {
        use tracing_subscriber::layer::SubscriberExt;
        let d = dir();
        // The process's ring is shared by the tests of this crate: only this one turns the mirror on.
        tail::persist(&d.data);
        let subscriber = tracing_subscriber::registry().with(tail::layer(|| false));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "sidevoice_core::x", "too quiet");
            for n in 0..30 {
                tracing::error!(target: "sidevoice_core::y", device = "d-77", n, "fill");
            }
        });
        // A new process: reads the file before writing its own.
        let lines = tail::render_previous(&tail::previous(&d.data));
        assert_eq!(lines.len(), TAIL_LINES);
        assert!(
            lines.last().unwrap().ends_with("fill device=d1 n=29"),
            "{lines:?}"
        );
        assert!(lines.iter().all(|line| !line.contains("d-77")));
        assert!(
            std::fs::metadata(d.data.join("crash-tail.json"))
                .unwrap()
                .len()
                < 256 * 1024
        );
        // A torn file is no tail; a foreign target is not taken.
        std::fs::write(d.data.join("crash-tail.json"), b"[{\"at\":1,\"lev").unwrap();
        assert!(tail::previous(&d.data).is_empty());
        std::fs::write(
            d.data.join("crash-tail.json"),
            br#"[{"at":1,"level":"ERROR","target":"reqwest::x","spans":[],"message":"m","fields":[]}]"#,
        )
        .unwrap();
        assert!(tail::previous(&d.data).is_empty());
    }
}
