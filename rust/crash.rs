//! Crash reports: what a panic of this core leaves on the machine, and its sending to Sentry at the next start.
//!
//! Decided with the operator (sidevoice/sidevoice-desktop#523): the core reports **its own** panics, on by default and never
//! asked, under one machine setting (`settings`, "send crash reports"; `sidevoice-core crash-reports on|off|status`, and
//! `/api/local/crash-reports` for the desktop app on the same machine, one way, app to core). Nothing relays through an app,
//! no paired device or agent can list, change or acknowledge a record, and nothing leaves a machine set to off: records
//! are then not made, the pending ones and the machine id are erased, and a send in flight stops at its next look.
//!
//! - A panic is captured ([`capture`]) by the panic hook: the frames of the dying thread (addresses only, `trace`), then a
//!   thread of its own resolves them, matches the loaded modules that hold a frame, takes the tail of our own warnings and
//!   errors (`tail`, an in-memory ring) and writes one record (`record`, `store`: private files, at most 20, 1 MiB and
//!   30 days, the same signature merged with a count). The hook stays safe (`docs/logging.md`, section 9): one capture at a
//!   time, locks only tried, a panic in the capture thread is contained there, the wait is bounded, and the default hook is
//!   never chained.
//! - At the next start, once the core is serving, a thread of its own ([`start`], `report`) sends each record to Sentry as an
//!   event built from the record's closed field list and deletes it once Sentry took it. No DSN in the build (a local one,
//!   a pull request) means nothing is sent, silently.
//! - Handled errors (`handled`) go out under the same switch: the `error` events of our own targets, one event per `error.type`
//!   per day, each with its code and place (the callsite's file and line) and the closed tags, never a message or a field value.
//!   A layer of the subscriber queues them, a thread of its own sends them through the same door as the crash reports.
//!
//! - A run that did not end by itself and whose manager saw a crash status (a signal, an out-of-memory kill, a Windows exception)
//!   leaves one more record at the next start (`exit`, decided in sidevoice-connector#366): a fixed type, the persisted tail of
//!   that run (`tail`, mirrored to a file), no frames; once a day per version and type, and never when a panic record of that
//!   run exists. The classification and the exit file are `sidevoice_platform::exit`'s.
//!
//! A record holds a closed list of fields ([`CrashRecord`]) and nothing else; text in it is bounded and free of control
//! characters. The panic message is only ever a `&'static str` literal or the fixed marker (with the formatted message's
//! length); the formatted message of `docs/logging.md` section 9 is never stored.

mod exit;
mod handled;
mod record;
mod report;
mod settings;
mod store;
mod tail;
#[cfg(test)]
mod tests;
mod trace;

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

pub use handled::{layer as handled_layer, unreported as unreported_errors, TAGS as HANDLED_TAGS};
pub use record::{CrashRecord, Exception, Frame, Image, Location, Panic};
pub use report::{event, EXTRAS, SERVICE, TAGS};
pub use settings::{enabled, read as setting};
pub use store::CrashStore;
pub use tail::layer as tail_layer;

/// The core's own log targets: what a record's tail keeps and what the handled errors are taken from. Not `panic` and
/// `crash_backtrace` (the hook's own, which the record itself replaces: a handled error is never one of those).
pub const OWN_TARGETS: [&str; 2] = ["sidevoice_core", "sidevoice_platform"];
/// The targets the panic hook logs under: never a handled error, never in a tail.
pub const PANIC_TARGETS: [&str; 2] = ["panic", "crash_backtrace"];

/// The most records kept.
pub const MAX_RECORDS: usize = 20;
/// The most bytes the records take together.
pub const MAX_BYTES: u64 = 1024 * 1024;
/// The longest a record is kept, from its last occurrence.
pub const MAX_AGE_DAYS: i64 = 30;
/// The most bytes one record file may have: a bigger one is not ours and is deleted.
pub const MAX_RECORD_BYTES: u64 = 64 * 1024;
/// The most events of the core's own log a record carries: its last warnings and errors.
pub const TAIL_LINES: usize = 20;
/// The most frames of the backtrace a record carries.
pub const MAX_FRAMES: usize = 64;
/// The most loaded modules a record carries: the ones that hold a frame.
pub const MAX_IMAGES: usize = 16;
/// How long the panic hook waits for the capture thread in all.
const HOOK_WAIT: Duration = Duration::from_secs(5);

/// What the capture needs and cannot ask for while the process is panicking.
struct Context {
    data: PathBuf,
    store: CrashStore,
    homes: Vec<String>,
    /// `os.type`, `os.name` and `host.arch` in OpenTelemetry's values.
    os: &'static str,
    os_name: &'static str,
    arch: &'static str,
    /// The loaded modules, listed at start.
    images: Vec<Image>,
    /// The sends under way (`report::deliver`): one count for the core, which has one context.
    in_flight: AtomicUsize,
    /// This run's random id: it ties the marker to the panic that may suppress an exit record. Never sent.
    run: String,
    /// The previous run, when it ended in a crash status (`exit`): its record is made once the sender starts.
    previous: Mutex<Option<exit::Previous>>,
}

static CONTEXT: OnceLock<Context> = OnceLock::new();
/// One capture at a time, process-wide: a panic that comes while one is under way (another thread's, or the capture's own)
/// is not recorded.
static CAPTURING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set on the sender's thread: a panic of the reporter itself is logged and contained, never recorded (no loop).
    static SENDING: Cell<bool> = const { Cell::new(false) };
}

/// From now on a panic of this process leaves a record. `data` is the core's private directory; `homes` are the spellings
/// of the home directory, each shown as `~` (the log's own backstop, reused here). Lists the loaded modules now.
pub fn install(
    data: &Path,
    homes: Vec<String>,
    os: &'static str,
    os_name: &'static str,
    arch: &'static str,
) {
    let run = uuid::Uuid::new_v4().simple().to_string();
    // Before the mirror is turned on: the previous run's tail is read first.
    let previous = exit::begin(data, &run, env!("CARGO_PKG_VERSION"));
    tail::persist(data);
    let _ = CONTEXT.set(Context {
        data: data.to_path_buf(),
        store: CrashStore::new(data, homes.clone()),
        homes,
        os,
        os_name,
        arch,
        images: trace::loaded(),
        in_flight: AtomicUsize::new(0),
        run,
        previous: Mutex::new(previous),
    });
}

/// Keeps the choice. Off first writes the switch (so a send in flight stops at its next look, and nothing new is recorded), then
/// erases the machine id and every pending record; a failure to erase is reported and the switch stays off (the next start
/// erases again).
pub fn set_enabled(data: &Path, on: bool) -> io::Result<()> {
    settings::write(data, on)?;
    if !on {
        erase(data)?;
    }
    Ok(())
}

/// Deletes the machine id, the day's cap of handled errors and every pending record.
fn erase(data: &Path) -> io::Result<()> {
    let id = settings::remove_id(data);
    let cap = handled::remove_cap(data);
    let records = CrashStore::new(data, Vec::new()).erase();
    id.and(cap)
        .and(exit::remove_cap(data))
        .and(tail::remove_mirror(data))
        .and(records)
}

/// Sends what a previous run left, and from now on the handled errors, in the background (call once the core is serving).
/// Never blocks and never fails.
pub fn start() {
    if let Some(context) = CONTEXT.get() {
        report::send_at_start(context);
        handled::send_from_now_on(context);
    }
}

struct Release;

impl Drop for Release {
    fn drop(&mut self) {
        CAPTURING.store(false, Ordering::SeqCst);
    }
}

/// Records a panic. Best effort and never panics: it is called by the panic hook, where a panic aborts the process and
/// cannot be caught, so only the frames' addresses (the dying thread's) are taken here; a thread of its own does the rest,
/// and the hook waits for it for [`HOOK_WAIT`] at most.
pub fn capture(literal: Option<&str>, formatted_len: usize, file: &str, line: u32) {
    let Some(context) = CONTEXT.get() else {
        return;
    };
    if SENDING.get() || CAPTURING.swap(true, Ordering::SeqCst) {
        return;
    }
    let trace = backtrace::Backtrace::new_unresolved();
    let (literal, file) = (literal.map(str::to_owned), file.to_owned());
    let (done, finished) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("crash-capture".into())
        .spawn(move || {
            let release = Release;
            write(
                context,
                literal.as_deref(),
                formatted_len,
                &file,
                line,
                trace,
            );
            // Released before the hook is told, so that the next panic, right after this one, finds the capture free. A panic
            // in `write` releases it as it unwinds.
            drop(release);
            let _ = done.send(());
        });
    match spawned {
        Ok(_) => {
            let _ = finished.recv_timeout(HOOK_WAIT);
        }
        Err(_) => CAPTURING.store(false, Ordering::SeqCst),
    }
}

/// The capture thread's work: nothing is made while the switch is off.
fn write(
    context: &Context,
    literal: Option<&str>,
    formatted_len: usize,
    file: &str,
    line: u32,
    mut trace: backtrace::Backtrace,
) {
    if !settings::enabled(&context.data) {
        return;
    }
    trace.resolve();
    let frames = trace::frames(&trace);
    let images = trace::holding(&context.images, &frames);
    let crashed = SystemTime::now();
    let record = CrashRecord::from_panic(
        Panic {
            version: env!("CARGO_PKG_VERSION"),
            os: context.os,
            os_name: context.os_name,
            arch: context.arch,
            at: crashed.into(),
            literal,
            formatted_len,
            file,
            line,
            frames,
            images,
            tail: tail::snapshot(crashed),
        },
        &context.homes,
    );
    // A thread that panicked inside the store holds its lock: do not wait for it.
    context.store.record_without_waiting(record);
    // The exit record of this run, if it ends in a crash status after all, is suppressed: this record says more.
    let _ = sidevoice_platform::exit::Marker::panicked(&context.data, &context.run);
}

/// The run ends by itself (`run` returned): no run marker is left, so the next start knows this one did not crash.
pub fn finish() {
    if let Some(context) = CONTEXT.get() {
        exit::finish(&context.data);
    }
}
