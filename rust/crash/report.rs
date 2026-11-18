//! A record as a Sentry event, the POST, and the pass that sends what a previous run left, at start.
//!
//! The event is built from the record's closed field list and nothing else ([`event`]; no SDK client, scope, integration or
//! transport exists in the process, only the SDK's event and envelope types). Sending never breaks the core: the pass runs on a thread
//! of its own, every POST has a timeout and the pass a budget, at most [`MAX_IN_FLIGHT`] sends are under way, a panic is
//! contained, and whatever fails is one log line; the record stays for the next start.
//!
//! [`deliver`] is the one door anything leaves through, and it asks the switch again just before the POST. What is
//! sent later under the same switch (the handled errors) comes in through it, as events of its own: never from the `panic` or
//! `crash_backtrace` targets, which this module already reports.

use std::borrow::Cow;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use sentry::protocol::{
    Addr, DebugImage, DebugMeta, Event, Exception, Frame, Level, Map, Stacktrace,
    SymbolicDebugImage, Value, Values,
};
use sentry::types::{CodeId, DebugId, Dsn, Uuid};
use sentry::Envelope;
use tracing::{info, warn};

use super::record::CrashRecord;
use super::{settings, Context, SENDING};
use crate::error::CoreError;

/// The `service.name` and the release's name: `sidevoice-core@<version>`.
pub const SERVICE: &str = "sidevoice-core";
/// How long one POST may take.
const POST_BUDGET: Duration = Duration::from_secs(10);
/// How long a whole pass may take; what is left waits for the next start.
const PASS_BUDGET: Duration = Duration::from_secs(60);
/// How many sends may be under way at once, whoever asks.
const MAX_IN_FLIGHT: usize = 2;

/// The tags an event may carry.
pub const TAGS: [&str; 4] = ["service.name", "os.type", "host.arch", "installation.id"];
/// The `extra` entries an event may carry.
pub const EXTRAS: [&str; 3] = ["location", "log_tail", "len"];

/// The event of a record, from its closed field list: the Sentry `event_id` is the record's id, the release the core's
/// version, and `installation.id` the machine id.
pub fn event(record: &CrashRecord, machine_id: &str) -> Event<'static> {
    let time = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text).map_or(SystemTime::now(), SystemTime::from)
    };
    let tags: Map<String, String> = [
        ("service.name", SERVICE),
        ("os.type", record.os.as_str()),
        ("host.arch", record.arch.as_str()),
        ("installation.id", machine_id),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    let frames: Vec<Frame> = record
        .backtrace
        .iter()
        .map(|frame| Frame {
            function: frame.function.clone(),
            filename: frame.file.clone(),
            lineno: frame.line.map(u64::from),
            instruction_addr: Some(Addr(frame.addr)),
            ..Default::default()
        })
        .collect();
    let images: Vec<DebugImage> = record
        .images
        .iter()
        .filter_map(|image| {
            Some(DebugImage::Symbolic(SymbolicDebugImage {
                name: image.name.clone(),
                arch: None,
                image_addr: Addr(image.addr),
                image_size: image.size,
                image_vmaddr: Addr(image.vmaddr),
                id: DebugId::from_str(&image.id).ok()?,
                code_id: image
                    .code_id
                    .as_deref()
                    .map(|id| CodeId::new(id.to_owned())),
                debug_file: None,
            }))
        })
        .collect();
    let mut extra: Map<String, Value> = Map::new();
    extra.insert(
        "location".to_owned(),
        Value::from(format!("{}:{}", record.location.file, record.location.line)),
    );
    extra.insert("log_tail".to_owned(), Value::from(record.log_tail.clone()));
    if let Some(len) = record.exception.len {
        extra.insert("len".to_owned(), Value::from(len));
    }
    Event {
        event_id: Uuid::parse_str(&record.id).unwrap_or_else(|_| Uuid::new_v4()),
        level: Level::Fatal,
        platform: Cow::Borrowed("native"),
        // Never the host name: the SDK's client would fill it in, and no client exists here; still said, not assumed.
        server_name: None,
        timestamp: time(&record.last_at),
        release: Some(Cow::Owned(format!("{SERVICE}@{}", record.version))),
        tags,
        extra,
        exception: Values::from(vec![Exception {
            ty: record.exception.kind.clone(),
            value: Some(record.exception.message.clone()),
            stacktrace: Stacktrace::from_frames_reversed(frames),
            ..Default::default()
        }]),
        debug_meta: Cow::Owned(DebugMeta {
            sdk_info: None,
            images,
        }),
        ..Default::default()
    }
}

/// Why an event did not leave.
#[derive(Debug)]
pub(super) enum Undelivered {
    /// The switch is off (or cannot be read): nothing leaves.
    Off,
    /// Sends are already under way.
    Busy,
    /// The POST failed (no network, a refusal, a rate limit, a timeout).
    Failed,
}

fn counter() -> &'static AtomicUsize {
    static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    &IN_FLIGHT
}

/// The one door: the switch is read again, a slot taken, and the envelope posted within [`POST_BUDGET`].
pub(super) async fn deliver(
    context: &Context,
    client: &reqwest::Client,
    dsn: &Dsn,
    event: Event<'static>,
) -> Result<(), Undelivered> {
    if !settings::enabled(&context.data) {
        return Err(Undelivered::Off);
    }
    let _slot = take().ok_or(Undelivered::Busy)?;
    let mut body = Vec::new();
    Envelope::from(event)
        .to_writer(&mut body)
        .map_err(|_| Undelivered::Failed)?;
    let sent = client
        .post(dsn.envelope_api_url().to_string())
        .header(
            "X-Sentry-Auth",
            dsn.to_auth(Some(concat!("sidevoice-core/", env!("CARGO_PKG_VERSION"))))
                .to_string(),
        )
        .header("Content-Type", "application/x-sentry-envelope")
        .body(body)
        .send()
        .await;
    match sent {
        Ok(answer) if answer.status().is_success() => Ok(()),
        _ => Err(Undelivered::Failed),
    }
}

fn take() -> Option<Guard> {
    if counter().fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
        counter().fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    Some(Guard)
}

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        counter().fetch_sub(1, Ordering::SeqCst);
    }
}

/// The records a previous run left, sent one at a time, newest first. A record is deleted once Sentry took it; the first
/// failure ends the pass (the network is likely down, and every record would wait out its timeout). Returns how many were sent.
pub(super) async fn drain(context: &Context, dsn: &Dsn) -> usize {
    let mut sent = 0;
    let Ok(client) = reqwest::Client::builder().timeout(POST_BUDGET).build() else {
        return sent;
    };
    let records = match context.store.pending() {
        Ok(records) => records,
        Err(error) => {
            warn!(
                error.type = CoreError::CrashReportsUnavailable.code(),
                kind = ?error.kind(),
                "the pending crash reports could not be read"
            );
            return sent;
        }
    };
    for record in records {
        let id = match settings::read(&context.data).and_then(|on| {
            if on {
                settings::machine_id(&context.data)
            } else {
                Err(std::io::ErrorKind::PermissionDenied.into())
            }
        }) {
            Ok(id) => id,
            // Off, or no id to be had: nothing is sent.
            Err(_) => break,
        };
        match deliver(context, &client, dsn, event(&record, &id)).await {
            Ok(()) => {
                sent += 1;
                if let Err(error) = context.store.remove(&record.id) {
                    warn!(
                        error.type = CoreError::CrashReportsUnavailable.code(),
                        kind = ?error.kind(),
                        "a sent crash report could not be deleted; it will be sent again under the same id"
                    );
                }
            }
            Err(Undelivered::Off) => break,
            Err(outcome) => {
                warn!(
                    error.type = CoreError::CrashReportFailed.code(),
                    outcome = ?outcome,
                    "a crash report could not be sent; it stays for the next start"
                );
                break;
            }
        }
    }
    sent
}

/// What the build was given: the DSN, set by the release and nightly workflows from the organisation's variable
/// `SENTRY_DSN`. None (a local build, a pull request), or one that does not parse, means nothing is ever sent, silently.
fn dsn() -> Option<Dsn> {
    option_env!("SIDEVOICE_SENTRY_DSN")
        .filter(|dsn| !dsn.is_empty())
        .and_then(|dsn| Dsn::from_str(dsn).ok())
}

/// The pass at start, on a thread of its own so that nothing here can hold the core up or take it down.
pub(super) fn send_at_start(context: &'static Context) {
    let _ = std::thread::Builder::new()
        .name("crash-send".into())
        .spawn(move || {
            SENDING.set(true);
            if catch_unwind(AssertUnwindSafe(|| pass(context, dsn().as_ref()))).is_err() {
                warn!(
                    error.type = CoreError::CrashReportFailed.code(),
                    "the crash reporter failed and was contained; the core goes on"
                );
            }
        });
}

pub(super) fn pass(context: &Context, dsn: Option<&Dsn>) {
    match settings::read(&context.data) {
        Ok(true) => {}
        // Records made while off, or left from before it, are discarded.
        Ok(false) => {
            if let Err(error) = super::erase(&context.data) {
                warn!(
                    error.type = CoreError::CrashReportsUnavailable.code(),
                    kind = ?error.kind(),
                    "the crash reports of a machine set not to send could not be deleted"
                );
            }
            return;
        }
        Err(error) => {
            warn!(
                error.type = CoreError::CrashReportsUnavailable.code(),
                kind = ?error.kind(),
                "the crash report setting could not be read; nothing is recorded or sent"
            );
            return;
        }
    }
    let Some(dsn) = dsn else { return };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let sent = runtime
        .block_on(tokio::time::timeout(PASS_BUDGET, drain(context, dsn)))
        .unwrap_or(0);
    if sent > 0 {
        info!(sent, "crash reports from a previous run were sent");
    }
}
