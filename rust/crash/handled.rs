//! Handled errors: the `error` events of our own targets, sent to Sentry under the crash-report switch
//! (`docs/logging.md`, section 10).
//!
//! What is sent is decided here and by construction, not by filtering a bigger event afterwards:
//!
//! - **Which events.** `error` level, a target of ours ([`OWN_TARGETS`]), never the panic hook's own (`PANIC_TARGETS`, written
//!   out and checked first: a target is matched whole or up to a `::`, so a lookalike such as `panic_x` is not ours either).
//!   The core has no webview target. One that carries no `error.type`, or one that is not a code of the `<component>.<name>`
//!   shape, is not sent: it is counted ([`untyped`]) and nothing else.
//! - **What of an event.** The layer reads exactly one field of it, `error.type`, and only as a string. The message and every
//!   other field value are never looked at, so nothing can leak from them. The place is the callsite's own file and line
//!   (`code.file.path`, `code.line.number`), compiled into the binary, as a short relative path or `unknown`. The sent event
//!   ([`event`]) is the release, the five tags of [`TAGS`] and one exception with that type and that one frame: no message,
//!   breadcrumb, extra, context, user or host name.
//! - **How often.** One event per `error.type` per day (UTC) per install, the same type again that day is dropped. The day's
//!   list lives in memory and in `handled-errors.json` (private, in the core's data directory, read at start and written
//!   after a send); a different day, a clock that went back or a file that says another day all mean a new day. At most
//!   [`MAX_TYPES`] types a day, so neither a storm of codes nor a bad file can make the list or the day's sends unbounded.
//! - **Under the switch.** The layer asks it before queueing: off, or a setting that cannot be read, queues nothing. The
//!   sender asks it again, and [`deliver`] a third time just before the POST. Off erases the day's list with the machine id.
//! - **Never breaks the core.** The layer does no network and no allocation beyond the code's own text: it checks, reads the
//!   switch and `try_send`s into a queue of [`QUEUE`] (a full queue drops the event). The POST is on a thread of its own with
//!   a runtime of its own, one at a time, [`POST_BUDGET`] each, `catch_unwind` around each event, and after a failed send it
//!   waits [`QUIET`] before trying again (an error that comes with the network down comes in storms). That thread never feeds
//!   itself: what it logs is `warn`, and the layer ignores events of a sending thread.

use std::borrow::Cow;
use std::fmt::Debug;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use chrono::{NaiveDate, Utc};
use sentry::protocol::{Event, Exception, Frame, Level, Stacktrace, Values};
use sentry::types::{Dsn, Uuid};
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{warn, Event as Logged, Metadata, Subscriber};
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Context as Scope, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::record::relative_path;
use super::report::{deliver, dsn, release, tags, Undelivered, POST_BUDGET, SERVICE};
use super::{settings, Context, OWN_TARGETS, PANIC_TARGETS, SENDING};
use crate::error::CoreError;
use crate::platform::fs::{Files, Fs};

/// The tags a handled error's event may carry.
pub const TAGS: [&str; 5] = [
    "service.name",
    "os.type",
    "os.name",
    "host.arch",
    "installation.id",
];
/// How many events wait for the sender.
const QUEUE: usize = 16;
/// How many error types one day's list holds.
pub(super) const MAX_TYPES: usize = 64;
/// The longest `error.type` taken.
const MAX_TYPE: usize = 64;
/// How long the sender leaves the network alone after a failed send.
const QUIET: Duration = Duration::from_secs(10 * 60);
/// The day's list, a private file of the core's data directory.
const CAP: &str = "handled-errors.json";
/// The most the file may hold when read: `MAX_TYPES` codes of `MAX_TYPE` characters, with room.
const LIMIT: u64 = 8 * 1024;

/// A handled error, as much of it as may leave: its code and the place of the `error!` that logged it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Handled {
    pub kind: String,
    pub file: String,
    pub line: u32,
}

/// The events without a usable `error.type` seen by this process.
static UNTYPED: AtomicU64 = AtomicU64::new(0);

/// How many `error` events of ours had no usable `error.type` since this process started. They are not sent; this is all
/// that is kept of them.
pub fn untyped() -> u64 {
    UNTYPED.load(Ordering::Relaxed)
}

/// What the layer queues into, until the sender takes it (`send_from_now_on`).
static PENDING: Mutex<Option<Receiver<Handled>>> = Mutex::new(None);

/// The layer of the process: `None` (nothing is ever queued) when the core has no data directory to read the switch from or the
/// build has no DSN, silently, as for the crash reports.
pub fn layer<S>(in_hook: fn() -> bool) -> Option<impl Layer<S>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let context = super::CONTEXT.get()?;
    dsn()?;
    let (queue, waiting) = sync_channel(QUEUE);
    *PENDING.lock().unwrap_or_else(PoisonError::into_inner) = Some(waiting);
    Some(queueing(context.data.clone(), queue, &UNTYPED, in_hook))
}

/// The layer, with its filter: events at `error` of our targets, the panic hook's own never.
pub(super) fn queueing<S>(
    data: PathBuf,
    queue: SyncSender<Handled>,
    untyped: &'static AtomicU64,
    in_hook: fn() -> bool,
) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    Queueing {
        data,
        queue,
        untyped,
        in_hook,
    }
    .with_filter(filter_fn(handled as fn(&Metadata<'_>) -> bool))
}

fn handled(meta: &Metadata<'_>) -> bool {
    let target = meta.target();
    meta.is_event()
        && *meta.level() == tracing::Level::ERROR
        && !in_targets(target, &PANIC_TARGETS)
        && in_targets(target, &OWN_TARGETS)
}

/// Whether `target` is one of `names` or below it (`name::module`): never a lookalike that only shares a prefix.
fn in_targets(target: &str, names: &[&str]) -> bool {
    names.iter().any(|name| {
        target == *name
            || target
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

/// Whether `text` is a code as the guideline writes them, `<component>.<kebab-name>`: short, lowercase, digits and `-`, with
/// a dot. Text that is not (a sentence, a path, a name somebody typed) is never an `error.type` that leaves.
pub(super) fn is_code(text: &str) -> bool {
    text.len() <= MAX_TYPE
        && text.split('.').count() >= 2
        && text.split('.').all(|part| {
            !part.is_empty()
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Reads one field of an event, `error.type`, and only when it is a string.
#[derive(Default)]
struct ErrorType(Option<String>);

impl Visit for ErrorType {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "error.type" && value.len() <= MAX_TYPE {
            self.0 = Some(value.to_owned());
        }
    }

    /// Every other field, and any other way of recording `error.type`, is not looked at.
    fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
}

struct Queueing {
    data: PathBuf,
    queue: SyncSender<Handled>,
    untyped: &'static AtomicU64,
    /// Whether this thread is inside the panic hook: nothing is done there.
    in_hook: fn() -> bool,
}

impl<S: Subscriber> Layer<S> for Queueing {
    fn on_event(&self, event: &Logged<'_>, _: Scope<'_, S>) {
        // The sender's own thread is never a source (no loop), nor is the panic hook's.
        if (self.in_hook)() || SENDING.get() {
            return;
        }
        let mut field = ErrorType::default();
        event.record(&mut field);
        let Some(kind) = field.0.filter(|kind| is_code(kind)) else {
            self.untyped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // Off, or a setting that cannot be read: nothing is queued.
        if !settings::enabled(&self.data) {
            return;
        }
        let meta = event.metadata();
        // A full queue drops the event: the logging thread never waits.
        let _ = self.queue.try_send(Handled {
            kind,
            file: relative_path(meta.file().unwrap_or("")),
            line: meta.line().unwrap_or(0),
        });
    }
}

/// The event of a handled error, from its closed field list: a random Sentry `event_id`, the core's version as the release,
/// the tags of [`TAGS`] and one exception, the code, with one frame (the place). `server_name` is unset; nothing else is.
pub(super) fn event(context: &Context, error: &Handled, machine_id: &str) -> Event<'static> {
    let frame = Frame {
        filename: Some(error.file.clone()),
        lineno: Some(u64::from(error.line)),
        ..Default::default()
    };
    Event {
        event_id: Uuid::new_v4(),
        level: Level::Error,
        platform: Cow::Borrowed("native"),
        // Never the host name: said, not assumed (no client exists to fill it in).
        server_name: None,
        timestamp: SystemTime::now(),
        release: Some(release(env!("CARGO_PKG_VERSION"))),
        tags: tags([
            ("service.name", SERVICE),
            ("os.type", context.os),
            ("os.name", context.os_name),
            ("host.arch", context.arch),
            ("installation.id", machine_id),
        ]),
        exception: Values::from(vec![Exception {
            ty: error.kind.clone(),
            value: None,
            stacktrace: Stacktrace::from_frames_reversed(vec![frame]),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

/// The types sent today. The day is the UTC date: any other date than today's, earlier or later, is a new day.
#[derive(Default)]
struct Cap {
    day: Option<NaiveDate>,
    sent: Vec<String>,
}

/// The file's shape when written; read back loosely (a value that is not ours is dropped, not the whole file).
#[derive(Serialize)]
struct Kept {
    day: String,
    sent: Vec<String>,
}

impl Cap {
    /// What the file says about `today`: nothing for another day, a file that is not ours or cannot be read, and only valid
    /// codes, at most [`MAX_TYPES`], otherwise.
    fn load(data: &Path, today: NaiveDate) -> Self {
        let Ok(Some(bytes)) = Files::read_private(&data.join(CAP), LIMIT) else {
            return Self::default();
        };
        let Ok(kept) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Self::default();
        };
        let same_day = kept["day"]
            .as_str()
            .is_some_and(|day| NaiveDate::parse_from_str(day, "%Y-%m-%d") == Ok(today));
        if !same_day {
            return Self::default();
        }
        let mut sent: Vec<String> = Vec::new();
        let listed = kept["sent"].as_array().map_or(&[][..], Vec::as_slice);
        for kind in listed
            .iter()
            .filter_map(|kind| kind.as_str())
            .filter(|kind| is_code(kind))
        {
            if sent.len() < MAX_TYPES && !sent.iter().any(|known| known == kind) {
                sent.push(kind.to_owned());
            }
        }
        Self {
            day: Some(today),
            sent,
        }
    }

    /// Whether `kind` may be sent on `today`. A day that is not the one the list is for starts a new list.
    fn permits(&mut self, kind: &str, today: NaiveDate) -> bool {
        if self.day != Some(today) {
            *self = Self {
                day: Some(today),
                sent: Vec::new(),
            };
        }
        self.sent.len() < MAX_TYPES && !self.sent.iter().any(|sent| sent == kind)
    }

    /// Records that `kind` was sent (after `permits` said it might be).
    fn mark(&mut self, kind: &str) {
        self.sent.push(kind.to_owned());
    }

    fn save(&self, data: &Path) -> io::Result<()> {
        Files::write_json(
            &data.join(CAP),
            &Kept {
                day: self.day.map(|day| day.to_string()).unwrap_or_default(),
                sent: self.sent.clone(),
            },
        )
    }
}

/// Deletes the day's list.
pub(super) fn remove_cap(data: &Path) -> io::Result<()> {
    match std::fs::remove_file(data.join(CAP)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Why a handled error was or was not sent.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Sent,
    /// The switch is off, or cannot be read.
    Off,
    /// Its type was already sent today, or the day's list is full.
    Capped,
    /// The network failed a moment ago.
    Quiet,
    /// Two sends are under way already (the process allows two at once): not sent now, not counted.
    Busy,
    /// Not sent now: no machine id, or the POST failed.
    Unsent,
}

/// The sender: one event at a time, under the switch, the day's cap and the quiet after a failure.
pub(super) struct Worker<'a> {
    context: &'a Context,
    client: reqwest::Client,
    dsn: Dsn,
    cap: Cap,
    quiet_until: Option<Instant>,
}

impl<'a> Worker<'a> {
    pub(super) fn new(context: &'a Context, client: reqwest::Client, dsn: Dsn) -> Self {
        Self {
            context,
            client,
            dsn,
            cap: Cap::load(&context.data, Utc::now().date_naive()),
            quiet_until: None,
        }
    }

    /// Sends `error` unless the switch is off, its type is capped on `today` or the network failed a moment ago. The type is
    /// counted as sent only once Sentry took the event, so an error that came with the network down is reported when it comes
    /// again.
    pub(super) async fn handle(&mut self, error: Handled, today: NaiveDate) -> Outcome {
        let data = &self.context.data;
        if self.quiet_until.is_some_and(|until| Instant::now() < until) {
            return Outcome::Quiet;
        }
        if !matches!(settings::read(data), Ok(true)) {
            // What the machine kept is erased when it is turned off; what this process remembers goes with it.
            self.cap = Cap::default();
            return Outcome::Off;
        }
        if !self.cap.permits(&error.kind, today) {
            return Outcome::Capped;
        }
        let id = match settings::machine_id(data) {
            Ok(id) => id,
            Err(failure) => {
                warn!(
                    error.type = CoreError::CrashReportsUnavailable.code(),
                    kind = ?failure.kind(),
                    "the machine id could not be had, so a handled error was not reported"
                );
                return Outcome::Unsent;
            }
        };
        match deliver(
            self.context,
            &self.client,
            &self.dsn,
            event(self.context, &error, &id),
        )
        .await
        {
            Ok(()) => {
                self.cap.mark(&error.kind);
                self.keep();
                Outcome::Sent
            }
            Err(Undelivered::Off) => {
                self.cap = Cap::default();
                Outcome::Off
            }
            Err(Undelivered::Busy) => Outcome::Busy,
            Err(Undelivered::Failed) => {
                self.quiet_until = Some(Instant::now() + QUIET);
                warn!(
                    error.type = CoreError::CrashReportFailed.code(),
                    "a handled error could not be reported; none is tried again for a while"
                );
                Outcome::Unsent
            }
        }
    }

    /// Writes the day's list. A failure is one line and the list holds in memory until the process ends.
    fn keep(&mut self) {
        let data = &self.context.data;
        if let Err(failure) = self.cap.save(data) {
            warn!(
                error.type = CoreError::CrashReportsUnavailable.code(),
                kind = ?failure.kind(),
                "the day's list of reported errors could not be saved; it holds until the core stops"
            );
        }
        // Off may have been written after the switch was read and before this file was: nothing is kept for a machine set to off.
        if !settings::enabled(data) {
            let _ = remove_cap(data);
            self.cap = Cap::default();
        }
    }
}

/// Starts the sender, once the core is serving: it takes what the layer queued until now and every event after it. Nothing
/// when the layer was not made (no DSN).
pub(super) fn send_from_now_on(context: &'static Context) {
    let Some(waiting) = PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let Some(dsn) = dsn() else { return };
    let _ = std::thread::Builder::new()
        .name("crash-handled".into())
        .spawn(move || {
            SENDING.set(true);
            run(context, dsn, &waiting);
        });
}

/// The sender's loop: it ends only if the layer's queue is gone, and an event that makes it panic costs that event.
fn run(context: &Context, dsn: Dsn, waiting: &Receiver<Handled>) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let Ok(client) = reqwest::Client::builder().timeout(POST_BUDGET).build() else {
        return;
    };
    let mut worker = Worker::new(context, client, dsn);
    while let Ok(error) = waiting.recv() {
        let today = Utc::now().date_naive();
        if catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(worker.handle(error, today))
        }))
        .is_err()
        {
            warn!(
                error.type = CoreError::CrashReportFailed.code(),
                "the reporter of handled errors failed and was contained; the core goes on"
            );
        }
    }
}
