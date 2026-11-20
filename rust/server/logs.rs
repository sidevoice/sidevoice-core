//! `GET /api/logs?source=&limit=&after=&wait=`: the core's own log (`core.log` and the generations rotation keeps), or
//! with `source=connector` the connector's, read by a paired device in pages. Only on the bearer-protected path: not a
//! local path, so the Unix socket needs the bearer too.
//!
//! The connector's log is not read from disk here: the core asks the connector that is linked, with `logs.read` over
//! the v3 link it already has (the connector opens nothing). The connector pages its own file with the same reader
//! behaviour, so a cursor is the connector's and this side only passes it through and checks what comes back. With
//! no connector linked, or one that does not answer, the read is unreadable (503). A held read polls the connector
//! like it polls the file, one request per tick.
//!
//! A read never writes a log line, at any level: a viewer would feed itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use super::pairing::unpaired;
use super::refusal::{refuse, require_origin, Handled, Refusal};
use super::request::query;
use super::{AppState, AuthenticatedDevice};

mod reader;
#[cfg(test)]
mod tests;

use reader::{Line, Page, Refused, DEFAULT_LIMIT, MAX_LIMIT};

/// The longest a read holds on for a line, whatever it asked.
const MAX_WAIT: Duration = Duration::from_secs(25);
/// How often a held read looks again.
const POLL: Duration = Duration::from_millis(250);
/// How long the connector has to answer one `logs.read`.
const LINK_WAIT: Duration = Duration::from_secs(5);
/// The longest cursor a page may carry (the reader's own bound).
const MAX_CURSOR: usize = 128;
const WAITS_PER_DEVICE: usize = 4;
/// The reads a device may start in a window, holding or not: well above a viewer's four a second, and a bound on what
/// a device that only reads can ask of the disk.
const READS_PER_WINDOW: usize = 100;
const READ_WINDOW: Duration = Duration::from_secs(10);
/// The connector reads in flight at once, across all devices: the connector serves a bounded number of requests on the
/// link at a time, and a flood of them would drop the voice link that shares it. Over this, a read is refused.
const CONNECTOR_READS_IN_FLIGHT: usize = 2;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/logs", get(read))
}

/// The log's files, oldest first, the reads holding on for a line and how many each device has started lately.
pub(super) struct Logs {
    files: Arc<[PathBuf]>,
    waits: Mutex<HashMap<String, usize>>,
    /// Per device: when its window began and the reads started in it.
    reads: Mutex<HashMap<String, (Instant, usize)>>,
    /// Places for the `logs.read` requests being asked of the connector, whoever asks.
    in_flight: tokio::sync::Semaphore,
    max_wait: Duration,
    poll: Duration,
    reads_per_window: usize,
    read_window: Duration,
    /// What the connector gets to answer one `logs.read`.
    link_wait: Duration,
}

impl Logs {
    pub(super) fn new(files: Vec<PathBuf>) -> Self {
        Self {
            files: files.into(),
            waits: Mutex::default(),
            reads: Mutex::default(),
            in_flight: tokio::sync::Semaphore::new(CONNECTOR_READS_IN_FLIGHT),
            max_wait: MAX_WAIT,
            poll: POLL,
            reads_per_window: READS_PER_WINDOW,
            read_window: READ_WINDOW,
            link_wait: LINK_WAIT,
        }
    }

    #[cfg(test)]
    fn timed(mut self, max_wait: Duration, poll: Duration) -> Self {
        self.max_wait = max_wait;
        self.poll = poll;
        self
    }

    #[cfg(test)]
    fn limited(mut self, reads_per_window: usize, read_window: Duration) -> Self {
        self.reads_per_window = reads_per_window;
        self.read_window = read_window;
        self
    }

    #[cfg(test)]
    fn linked(mut self, link_wait: Duration) -> Self {
        self.link_wait = link_wait;
        self
    }

    /// Whether the device may start another read in this window.
    fn admit(&self, device: &str) -> bool {
        let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        reads.retain(|_, (began, _)| now.duration_since(*began) < self.read_window);
        let (_, count) = reads.entry(device.to_owned()).or_insert((now, 0));
        *count += 1;
        *count <= self.reads_per_window
    }

    /// A place among the device's holding reads, or `None` when it has all of them.
    fn hold<'a>(&'a self, device: &'a str) -> Option<Held<'a>> {
        let mut waits = self.waits.lock().unwrap_or_else(PoisonError::into_inner);
        let count = waits.entry(device.to_owned()).or_default();
        if *count >= WAITS_PER_DEVICE {
            return None;
        }
        *count += 1;
        Some(Held { logs: self, device })
    }
}

struct Held<'a> {
    logs: &'a Logs,
    device: &'a str,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let mut waits = self
            .logs
            .waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = waits.get_mut(self.device) {
            *count -= 1;
            if *count == 0 {
                waits.remove(self.device);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Core,
    Connector,
}

struct Request {
    source: Source,
    after: Option<String>,
    limit: usize,
    wait: Duration,
}

impl Request {
    fn parse(uri: &Uri, max_wait: Duration) -> Option<Self> {
        let limit = match query(uri, "limit") {
            Some(limit) => limit.parse().ok().filter(|n| (1..=MAX_LIMIT).contains(n))?,
            None => DEFAULT_LIMIT,
        };
        let wait = match query(uri, "wait") {
            Some(wait) => Duration::from_secs(wait.parse().ok()?),
            None => Duration::ZERO,
        };
        let source = match query(uri, "source").as_deref() {
            None | Some("core") => Source::Core,
            Some("connector") => Source::Connector,
            Some(_) => return None,
        };
        let after = query(uri, "after");
        // What goes over the link is bounded here, not trusted to the other side.
        if source == Source::Connector
            && after.as_ref().is_some_and(|after| after.len() > MAX_CURSOR)
        {
            return None;
        }
        Some(Self {
            source,
            after,
            limit,
            wait: wait.min(max_wait),
        })
    }
}

async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    device: Option<Extension<AuthenticatedDevice>>,
) -> Handled {
    require_origin(&headers)?;
    let Some(Extension(AuthenticatedDevice(device))) = device else {
        return Err(unpaired(&headers));
    };
    let logs = &state.logs;
    if !logs.admit(&device) {
        return Err(rate_limited(&headers));
    }
    let request = Request::parse(&uri, logs.max_wait)
        .ok_or_else(|| refuse("logs.request_invalid", StatusCode::BAD_REQUEST, &headers))?;
    let _held = if request.wait.is_zero() {
        None
    } else {
        Some(logs.hold(&device).ok_or_else(|| rate_limited(&headers))?)
    };
    let deadline = Instant::now() + request.wait;
    loop {
        let page = match request.source {
            Source::Core => {
                let (files, after, limit) =
                    (logs.files.clone(), request.after.clone(), request.limit);
                tokio::task::spawn_blocking(move || reader::read(&files, after.as_deref(), limit))
                    .await
                    .unwrap_or(Err(Refused::Unreadable))
            }
            Source::Connector => {
                let Ok(_place) = logs.in_flight.try_acquire() else {
                    return Err(rate_limited(&headers));
                };
                connector_page(&state, &request).await
            }
        };
        // Whoever was revoked while it held on gets nothing.
        if !state
            .registry
            .lock()
            .expect("registry lock")
            .contains(&device)
        {
            return Err(unpaired(&headers));
        }
        let page = page.map_err(|error| refused(error, &headers))?;
        let left = deadline.saturating_duration_since(Instant::now());
        if !page.lines.is_empty() || left.is_zero() {
            return Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)).into_response());
        }
        tokio::time::sleep(left.min(logs.poll)).await;
    }
}

fn rate_limited(headers: &HeaderMap) -> Refusal {
    refuse(
        "request.rate_limited",
        StatusCode::TOO_MANY_REQUESTS,
        headers,
    )
}

fn refused(error: Refused, headers: &HeaderMap) -> Refusal {
    let (key, status) = match error {
        Refused::Invalid => ("logs.request_invalid", StatusCode::BAD_REQUEST),
        Refused::Expired => ("logs.cursor_expired", StatusCode::GONE),
        Refused::Unreadable => ("logs.unreadable", StatusCode::SERVICE_UNAVAILABLE),
    };
    refuse(key, status, headers)
}

/// One page of the linked connector's log, asked over the link. Nothing the connector answers is trusted past its
/// shape: a page that is not one is unreadable, whatever it holds.
async fn connector_page(state: &AppState, request: &Request) -> Result<Page, Refused> {
    let peer = state.room.connector_peer().ok_or(Refused::Unreadable)?;
    let mut params = json!({"limit": request.limit});
    if let Some(after) = &request.after {
        params["after"] = json!(after);
    }
    let answer = peer
        .request("logs.read", params, state.logs.link_wait)
        .await
        .map_err(|_| Refused::Unreadable)?;
    if let Some(key) = answer["error"]["key"].as_str() {
        return Err(match key {
            "logs.request_invalid" => Refused::Invalid,
            "logs.cursor_expired" => Refused::Expired,
            _ => Refused::Unreadable,
        });
    }
    page_of(&answer, request.limit).ok_or(Refused::Unreadable)
}

fn page_of(answer: &Value, limit: usize) -> Option<Page> {
    let cursor = answer["cursor"]
        .as_str()
        .filter(|cursor| !cursor.is_empty() && cursor.len() <= MAX_CURSOR)?;
    let more = answer["more"].as_bool()?;
    let lines = answer["lines"]
        .as_array()
        .filter(|lines| lines.len() <= limit)?;
    let lines = lines
        .iter()
        .map(|line| {
            let id = line["id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= MAX_CURSOR)?;
            Some(Line {
                id: id.to_owned(),
                text: line["text"].as_str()?.to_owned(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Page {
        lines,
        cursor: cursor.to_owned(),
        more,
    })
}
