//! `GET /api/logs?limit=&after=&wait=`: the core's own log (`core.log` and the generations rotation keeps), read by a
//! paired device in pages. Only on the bearer-protected path: not a local path, so the Unix socket needs the bearer
//! too, and not a method of the connector link.
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

use super::pairing::unpaired;
use super::refusal::{refuse, require_origin, Handled, Refusal};
use super::request::query;
use super::{AppState, AuthenticatedDevice};

mod reader;
#[cfg(test)]
mod tests;

use reader::{Refused, DEFAULT_LIMIT, MAX_LIMIT};

/// The longest a read holds on for a line, whatever it asked.
const MAX_WAIT: Duration = Duration::from_secs(25);
/// How often a held read looks again.
const POLL: Duration = Duration::from_millis(250);
const WAITS_PER_DEVICE: usize = 4;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/logs", get(read))
}

/// The log's files, oldest first, and the reads holding on for a line.
pub(super) struct Logs {
    files: Arc<[PathBuf]>,
    waits: Mutex<HashMap<String, usize>>,
    max_wait: Duration,
    poll: Duration,
}

impl Logs {
    pub(super) fn new(files: Vec<PathBuf>) -> Self {
        Self {
            files: files.into(),
            waits: Mutex::default(),
            max_wait: MAX_WAIT,
            poll: POLL,
        }
    }

    #[cfg(test)]
    fn timed(mut self, max_wait: Duration, poll: Duration) -> Self {
        self.max_wait = max_wait;
        self.poll = poll;
        self
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

struct Request {
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
        Some(Self {
            after: query(uri, "after"),
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
    let request = Request::parse(&uri, logs.max_wait)
        .ok_or_else(|| refuse("logs.request_invalid", StatusCode::BAD_REQUEST, &headers))?;
    let _held = if request.wait.is_zero() {
        None
    } else {
        Some(logs.hold(&device).ok_or_else(|| {
            refuse(
                "request.rate_limited",
                StatusCode::TOO_MANY_REQUESTS,
                &headers,
            )
        })?)
    };
    let deadline = Instant::now() + request.wait;
    loop {
        let page = {
            let (files, after, limit) = (logs.files.clone(), request.after.clone(), request.limit);
            tokio::task::spawn_blocking(move || reader::read(&files, after.as_deref(), limit))
                .await
                .unwrap_or(Err(Refused::Unreadable))
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

fn refused(error: Refused, headers: &HeaderMap) -> Refusal {
    let (key, status) = match error {
        Refused::Invalid => ("logs.request_invalid", StatusCode::BAD_REQUEST),
        Refused::Expired => ("logs.cursor_expired", StatusCode::GONE),
        Refused::Unreadable => ("logs.unreadable", StatusCode::SERVICE_UNAVAILABLE),
    };
    refuse(key, status, headers)
}
