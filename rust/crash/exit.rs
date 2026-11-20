//! Abnormal exits: what the panic hook never sees (a signal, an abort, an out-of-memory kill, a Windows exception).
//!
//! Decided with the operator (sidevoice-connector#366, option B). Once the core holds its data directory's lock (`begin`, called
//! by `serve` right after it) it leaves a run marker there (`sidevoice_platform::exit::Marker`: a random run id that is never
//! sent) and removes it when `run` returns by itself ([`finish`], only a marker that is this run's). A marker still there at
//! the next start means the previous run did not end cleanly; the crash status the OS manager saw is then read (the
//! classification and the exit file are `sidevoice_platform::exit`'s, once for the core and the connector) and, only if it is a
//! crash status, one record is made **at once** with the persisted tail of that run (`tail`: already prepared, it holds its run's
//! id and is taken only for that run's marker): a core that crashes at startup is recorded too, only the sending waits for
//! the core to be serving. A panic of the same run suppresses the record only when the exit is the abort a panic produces
//! (`is_panic_abort`): the panic record is richer then; any other end is reported as well. A `SIGKILL`, a force quit and any
//! other status are never reported.
//!
//! No daily cap (operator, 2026-11-20): every abnormal exit is a record, as every panic is. The restart budget of the
//! service managers bounds a crash loop. The same switch, closed field list, store and sender as the panics' apply: nothing is
//! made while the switch is off.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sidevoice_platform::exit::{self as platform_exit, launchd, Marker};

use super::record::{Abnormal, CrashRecord};
use super::{settings, tail, Context};

/// The previous run, when it ended in a crash status worth a record.
pub(super) struct Previous {
    pub(super) version: String,
    pub(super) kind: String,
    /// The tail the previous run mirrored, prepared.
    pub(super) tail: Vec<String>,
}

/// What the previous run left, read at start, before the manager's own account of it (macOS: launchd, asked later) is known.
pub(super) struct Pending {
    version: String,
    launchd_runs: Option<u64>,
    panicked: bool,
    /// The crash type an exit file said.
    seen: Option<String>,
    tail: Vec<String>,
}

/// Where the core's exit file is: under `D`, the directory that holds the core's own (`D/core`, or whatever `--data-dir` names:
/// its parent), as the connector's service layer writes it.
pub fn exit_file_of(core_dir: &Path) -> Option<PathBuf> {
    Some(platform_exit::exit_file(
        core_dir.parent()?,
        &platform_exit::CORE,
    ))
}

/// Starts this run, which holds the data directory's lock: reads what the previous one left (its tail, before this run writes its
/// own), puts this run's marker and turns the tail mirror on. The record of a previous crash is made here, locally; only its
/// sending waits. Best effort: nothing here fails the core.
pub(super) fn begin(context: &'static Context) {
    begin_with(context, true);
}

/// [`begin`]; `mirror` turns the tail mirror on (the process has one: tests keep it for themselves).
fn begin_with(context: &'static Context, mirror: bool) {
    let pending = start(
        &context.data,
        &context.run,
        env!("CARGO_PKG_VERSION"),
        exit_file_of(&context.data),
    );
    if mirror {
        tail::persist(&context.data, &context.run, context.homes.clone());
    }
    if cfg!(target_os = "macos") {
        // launchd is asked off the start's way: a stall of `launchctl` must not hold the core up.
        let _ = std::thread::Builder::new()
            .name("crash-launchd".into())
            .spawn(move || {
                let state = launchd::print(platform_exit::CORE.label)
                    .ok()
                    .map(|text| launchd::parse(&text));
                if let Some(runs) = state.as_ref().and_then(|state| state.runs) {
                    let _ = Marker::launchd_runs(&context.data, &context.run, runs);
                }
                if let Some(previous) = pending.and_then(|pending| pending.decide(state.as_ref())) {
                    record(context, previous);
                }
            });
    } else if let Some(previous) = pending.and_then(|pending| pending.decide(None)) {
        record(context, previous);
    }
}

/// The part of [`begin`] that is local and cheap: the marker and the exit file.
fn start(data: &Path, run: &str, version: &str, exit_file: Option<PathBuf>) -> Option<Pending> {
    let current = Marker::new(run.to_owned(), version, None);
    let Some(marker) = Marker::begin(data, &current).ok().flatten() else {
        // A clean end or a first start: an exit file left by a crash status is about no run of ours.
        if let Some(path) = &exit_file {
            let _ = std::fs::remove_file(path);
        }
        return None;
    };
    // The exit file is read (and deleted) whatever the marker says, so that none is left for a later run.
    let seen = exit_file.and_then(|path| platform_exit::take(&path, marker.started));
    Some(Pending {
        version: marker.version,
        launchd_runs: marker.launchd_runs,
        panicked: marker.panicked,
        seen,
        tail: tail::previous(data, &marker.run),
    })
}

impl Pending {
    /// What the previous run's end was, if a crash status: from the exit file, or from launchd's `state` (only when its run count
    /// went up by exactly one). A panic record of that run stands for it when the end is the abort a panic produces.
    fn decide(self, state: Option<&launchd::State>) -> Option<Previous> {
        let kind = self
            .seen
            .or_else(|| state.and_then(|state| launchd::crash_type(self.launchd_runs, state)))?;
        if self.panicked && platform_exit::is_panic_abort(&kind) {
            return None;
        }
        Some(Previous {
            version: self.version,
            kind,
            tail: self.tail,
        })
    }
}

/// Run `run` ended by itself: its marker is removed, so the next start knows it did not crash. A marker that is another run's is
/// left alone.
pub(super) fn finish(data: &Path, run: &str) {
    let _ = Marker::end(data, run);
}

/// Makes the record of the previous run's exit (the switch allowing). Every exit is one: there is no cap.
fn record(context: &Context, previous: Previous) {
    if !settings::enabled(&context.data) {
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

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::crash::{tail, TAIL_LINES};
    use crate::platform::fs::{Files, Fs};
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
        let exit = exit_file_of(&data).unwrap();
        Dir {
            _temp: temp,
            data,
            exit,
        }
    }

    fn start_run(d: &Dir, run: &str, state: Option<launchd::State>) -> Option<Previous> {
        start(&d.data, run, "0.20.0", Some(d.exit.clone()))
            .and_then(|pending| pending.decide(state.as_ref()))
    }

    fn context(d: &Dir) -> Context {
        Context {
            data: d.data.clone(),
            store: crate::crash::CrashStore::new(&d.data, vec!["/home/ana".to_owned()]),
            homes: vec!["/home/ana".to_owned()],
            os: "linux",
            os_name: "Linux",
            arch: "amd64",
            images: Vec::new(),
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            run: "ab".to_owned(),
        }
    }

    #[test]
    fn the_exit_file_is_beside_the_data_directory_whatever_it_is_called() {
        let path = exit_file_of(Path::new("/srv/sv/custom")).unwrap();
        assert_eq!(path, Path::new("/srv/sv/service/sidevoice-core.exit"));
        assert_eq!(exit_file_of(Path::new("/")), None);
    }

    #[test]
    fn a_crash_status_after_a_run_that_did_not_end_makes_one_previous() {
        let d = dir();
        assert!(
            start_run(&d, "aa", None).is_none(),
            "the first start has no previous"
        );
        platform_exit::record(&d.exit, "ntstatus.0xC0000005").unwrap();
        let previous = start_run(&d, "bb", None).expect("a crash");
        assert_eq!(previous.kind, "ntstatus.0xC0000005");
        assert_eq!(previous.version, "0.20.0");
        // The exit file was read once.
        assert!(!d.exit.exists());
    }

    #[test]
    fn a_run_that_ended_by_itself_or_died_without_a_crash_status_reports_nothing() {
        let d = dir();
        start_run(&d, "aa", None);
        finish(&d.data, "aa");
        platform_exit::record(&d.exit, "abort.signal.SIGSEGV").unwrap();
        assert!(
            start_run(&d, "bb", None).is_none(),
            "no marker: a clean end"
        );
        // Killed (no crash status): the marker is left, nothing was recorded.
        assert!(start_run(&d, "cc", None).is_none());
    }

    #[test]
    fn a_second_start_that_lost_the_lock_leaves_the_live_runs_files_alone() {
        let d = dir();
        start_run(&d, "aa", None);
        // The live run's exit file would be another run's, and the second start never began: it only finishes.
        finish(&d.data, "zz");
        assert!(
            start_run(&d, "bb", None).is_none(),
            "no crash status was recorded"
        );
        // The marker of "aa" was still there for "bb" to find: its crash status is read.
        let d = dir();
        start_run(&d, "aa", None);
        finish(&d.data, "zz");
        platform_exit::record(&d.exit, "abort.oom_kill").unwrap();
        let previous = start_run(&d, "bb", None).expect("the marker survived");
        assert_eq!(previous.kind, "abort.oom_kill");
    }

    #[test]
    fn a_panic_suppresses_only_the_abort_it_produces() {
        for (kind, reported) in [
            ("abort.signal.SIGABRT", false),
            ("ntstatus.0xC0000409", false),
            ("abort.signal.SIGSEGV", true),
            ("abort.oom_kill", true),
            ("ntstatus.0xC0000005", true),
        ] {
            let d = dir();
            start_run(&d, "aa", None);
            Marker::panicked(&d.data, "aa").unwrap();
            platform_exit::record(&d.exit, kind).unwrap();
            assert_eq!(start_run(&d, "bb", None).is_some(), reported, "{kind}");
            assert!(!d.exit.exists(), "its exit file is gone all the same");
            // The next run's own exit is reported again.
            platform_exit::record(&d.exit, kind).unwrap();
            assert!(start_run(&d, "cc", None).is_some(), "{kind}");
        }
    }

    #[test]
    fn launchd_is_asked_only_when_its_run_count_went_up_by_one() {
        let d = dir();
        let state = |runs| launchd::State {
            runs: Some(runs),
            last_exit_code: None,
            last_signal: Some(11),
        };
        start_run(&d, "aa", Some(state(7)));
        // The count learned after the start is written into the marker.
        Marker::launchd_runs(&d.data, "aa", 7).unwrap();
        let previous = start_run(&d, "bb", Some(state(8))).expect("one more run");
        assert_eq!(previous.kind, "abort.signal.SIGSEGV");
        Marker::launchd_runs(&d.data, "bb", 8).unwrap();
        assert!(
            start_run(&d, "cc", Some(state(12))).is_none(),
            "several runs: not ours"
        );
    }

    #[test]
    fn every_exit_is_a_record_with_a_fixed_message_no_frames_and_no_personal_data() {
        let d = dir();
        let context = context(&d);
        let previous = |kind: &str, tail: Vec<String>| Previous {
            version: "0.20.0".to_owned(),
            kind: kind.to_owned(),
            tail,
        };
        record(
            &context,
            previous(
                "abort.signal.SIGSEGV",
                vec!["+0.000s WARN sidevoice_core::x: failed at /home/ana/secret".to_owned()],
            ),
        );
        let records = context.store.pending().unwrap();
        assert_eq!(records.len(), 1);
        let one = &records[0];
        assert_eq!(one.exception.kind, "abort.signal.SIGSEGV");
        assert_eq!(one.exception.message, "the process ended abnormally");
        assert!(one.backtrace.is_empty() && one.images.is_empty());
        let text = serde_json::to_string(&crate::crash::event(one, "m")).unwrap();
        assert!(!text.contains("ana"), "{text}");
        assert!(text.contains("~/secret"), "{text}");
        // No daily cap: the same type again, at once, is counted, not held back.
        record(&context, previous("abort.signal.SIGSEGV", Vec::new()));
        record(&context, previous("abort.signal.SIGSEGV", Vec::new()));
        let records = context.store.pending().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].count, 3);
        // And nothing is made while the switch is off.
        crate::crash::set_enabled(&d.data, false).unwrap();
        record(&context, previous("abort.oom_kill", Vec::new()));
        assert!(context.store.pending().unwrap().is_empty());
    }

    #[test]
    fn a_crash_at_startup_is_recorded_by_begin_before_anything_is_sent() {
        let d = dir();
        start_run(&d, "aa", None);
        platform_exit::record(&d.exit, "abort.signal.SIGSEGV").unwrap();
        // The next start: its `begin` makes the record (the store write is local); no sender is involved.
        let context: &'static Context = Box::leak(Box::new(context(&d)));
        begin_with(context, false);
        // On macOS launchd is asked on a thread off the start's way, so the record lands a moment later.
        let mut records = context.store.pending().unwrap();
        for _ in 0..200 {
            if !records.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
            records = context.store.pending().unwrap();
        }
        assert_eq!(records.len(), 1, "made before the core serves");
        assert_eq!(records[0].exception.kind, "abort.signal.SIGSEGV");
    }

    fn wait_for(path: &Path, text: &str) -> String {
        for _ in 0..200 {
            if let Ok(content) = std::fs::read_to_string(path) {
                if content.contains(text) && content.trim_end().ends_with("]}") {
                    return content;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("{text} never reached {path:?}");
    }

    #[test]
    fn the_mirror_is_prepared_bounded_tied_to_its_run_and_survives_a_restart() {
        use tracing_subscriber::layer::SubscriberExt;
        let _ring = tail::testing::LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail::testing::clear();
        let d = dir();
        // The process's ring and writer are shared by the tests of this crate: only this one turns the mirror on.
        std::fs::write(
            d.data.join("crash-tail.json"),
            br#"{"run":"old","lines":[]}"#,
        )
        .unwrap();
        tail::persist(&d.data, "r1", vec!["/home/ana".to_owned()]);
        assert!(
            !d.data.join("crash-tail.json").exists() || tail::previous(&d.data, "old").is_empty(),
            "the previous run's file is gone at start"
        );
        let subscriber = tracing_subscriber::registry().with(tail::layer(|| false));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "sidevoice_core::x", "too quiet");
            for n in 0..30 {
                tracing::error!(
                    target: "sidevoice_core::y", device = "d-77", n, path = "/home/ana/x", "fill"
                );
            }
        });
        let file = d.data.join("crash-tail.json");
        wait_for(&file, "n=29");
        // A new process whose marker is run r1: it reads the lines, already prepared.
        let lines = tail::previous(&d.data, "r1");
        assert_eq!(lines.len(), TAIL_LINES);
        let last = lines.last().unwrap();
        assert!(last.starts_with("+0.000s ERROR"), "{last}");
        assert!(last.contains("fill device=d1 n=29 path=\"~/x\""), "{last}");
        assert!(lines
            .iter()
            .all(|line| !line.contains("d-77") && !line.contains("ana")));
        assert!(std::fs::metadata(&file).unwrap().len() < 256 * 1024);
        // Old lines never attach to another run's crash.
        assert!(tail::previous(&d.data, "r2").is_empty());
        // A torn file is no tail; a foreign line is not taken.
        std::fs::write(&file, b"{\"run\":\"r1\",\"lin").unwrap();
        assert!(tail::previous(&d.data, "r1").is_empty());
        std::fs::write(
            &file,
            br#"{"run":"r1","lines":["not a line of ours","+1.0s ERROR reqwest::x: m"]}"#,
        )
        .unwrap();
        assert!(tail::previous(&d.data, "r1").is_empty());
    }
}
