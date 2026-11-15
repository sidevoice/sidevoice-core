//! The record's closed field list, its signature and the validation every record passes before it is kept.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{Panic, MAX_FRAMES, TAIL_LINES};

/// What stands in for a panic message that is not a literal (`docs/logging.md`, section 9).
pub const FORMATTED_MARKER: &str = "<formatted panic message omitted>";
const MAX_TEXT: usize = 200;
const MAX_FRAME: usize = 240;
const MAX_LINE: usize = 600;
/// How many of the first frames the signature takes.
const SIGNATURE_FRAMES: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Piece {
    Core,
    Connector,
}

impl Piece {
    fn name(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Connector => "connector",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exception {
    /// `panic`, or a stable error code.
    #[serde(rename = "type")]
    pub kind: String,
    /// A `&'static str` payload, or [`FORMATTED_MARKER`].
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub line: u32,
}

/// One crash, or the several of one signature (`count`, `first_at`, `last_at`). Exactly these fields are stored, listed
/// and sent on to an app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashRecord {
    /// A random UUID v4, made when the crash happened; the app may use it as the Sentry `event_id`.
    pub id: String,
    pub piece: Piece,
    pub version: String,
    pub os: String,
    pub arch: String,
    /// RFC 3339, UTC, milliseconds.
    pub first_at: String,
    pub last_at: String,
    pub count: u32,
    /// Groups the crashes that are the same: a hash of the piece, version, location and top frames.
    pub signature: String,
    pub exception: Exception,
    pub location: Location,
    pub backtrace: Vec<String>,
    /// The last lines of the piece's own log, masked where they were written.
    pub log_tail: Vec<String>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl CrashRecord {
    pub fn from_panic(
        piece: Piece,
        version: &str,
        os: &str,
        arch: &str,
        panic: &Panic<'_>,
        tail: &[String],
        homes: &[String],
    ) -> Self {
        let message = panic.literal.unwrap_or(FORMATTED_MARKER);
        let at = now();
        let hide = |text: &str| {
            homes
                .iter()
                .filter(|home| home.len() > 1)
                .fold(text.to_owned(), |text, home| {
                    text.replace(home.as_str(), "~")
                })
        };
        let mut record = Self {
            id: Uuid::new_v4().to_string(),
            piece,
            version: version.to_owned(),
            os: os.to_owned(),
            arch: arch.to_owned(),
            first_at: at.clone(),
            last_at: at,
            count: 1,
            signature: String::new(),
            exception: Exception {
                kind: "panic".to_owned(),
                message: hide(message),
            },
            location: Location {
                file: hide(panic.file),
                line: panic.line,
            },
            backtrace: frames(&hide(panic.backtrace)),
            log_tail: tail.iter().map(|line| hide(line)).collect(),
        };
        record.normalize();
        record
    }

    /// Reads a record from untrusted JSON: only the closed field list is taken, every text is bounded and cleaned, and
    /// the signature is made again rather than believed. `None` when a required field is missing or malformed.
    pub fn from_untrusted(value: &Value) -> Option<Self> {
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let mut record = Self {
            id: text(&value["id"])?,
            piece: serde_json::from_value(value["piece"].clone()).ok()?,
            version: text(&value["version"])?,
            os: text(&value["os"])?,
            arch: text(&value["arch"])?,
            first_at: text(&value["first_at"])?,
            last_at: text(&value["last_at"])?,
            count: value["count"]
                .as_u64()
                .map_or(1, |count| count.clamp(1, u64::from(u32::MAX)) as u32),
            signature: String::new(),
            exception: Exception {
                kind: text(&value["exception"]["type"])?,
                message: text(&value["exception"]["message"])?,
            },
            location: Location {
                file: text(&value["location"]["file"])?,
                line: u32::try_from(value["location"]["line"].as_u64()?).ok()?,
            },
            backtrace: lines(&value["backtrace"]),
            log_tail: lines(&value["log_tail"]),
        };
        if Uuid::parse_str(&record.id).ok()?.to_string() != record.id
            || !token(&record.version, 64, |c| {
                c.is_ascii_alphanumeric() || "._+-".contains(c)
            })
            || !token(&record.os, 32, |c| {
                c.is_ascii_alphanumeric() || "_-".contains(c)
            })
            || !token(&record.arch, 32, |c| {
                c.is_ascii_alphanumeric() || "_-".contains(c)
            })
            || !token(&record.exception.kind, 64, |c| {
                c.is_ascii_alphanumeric() || "._-".contains(c)
            })
            || chrono::DateTime::parse_from_rfc3339(&record.first_at).is_err()
            || chrono::DateTime::parse_from_rfc3339(&record.last_at).is_err()
        {
            return None;
        }
        record.normalize();
        Some(record)
    }

    /// Bounds every text and makes the signature.
    fn normalize(&mut self) {
        self.exception.message = clean(&self.exception.message, MAX_TEXT);
        self.location.file = clean(&self.location.file, MAX_TEXT);
        self.backtrace.truncate(MAX_FRAMES);
        for frame in &mut self.backtrace {
            *frame = clean(frame, MAX_FRAME);
        }
        let skip = self.log_tail.len().saturating_sub(TAIL_LINES);
        self.log_tail = self
            .log_tail
            .drain(skip..)
            .map(|line| clean(&line, MAX_LINE))
            .collect();
        self.signature = self.make_signature();
    }

    fn make_signature(&self) -> String {
        let mut hash = Sha256::new();
        for part in [
            self.piece.name(),
            &self.version,
            &self.location.file,
            &self.location.line.to_string(),
        ] {
            hash.update(part.as_bytes());
            hash.update([0]);
        }
        for frame in self.backtrace.iter().take(SIGNATURE_FRAMES) {
            hash.update(frame.as_bytes());
            hash.update([0]);
        }
        hash.finalize()[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

fn token(value: &str, max: usize, allowed: impl Fn(char) -> bool) -> bool {
    !value.is_empty() && value.len() <= max && value.chars().all(allowed)
}

/// `text` without control characters, cut to at most `max` characters.
fn clean(text: &str, max: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect()
}

fn lines(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The frames of a `Backtrace`'s `Display`: `   3: symbol` and, under it, `at file:line`, one string each.
pub(super) fn frames(backtrace: &str) -> Vec<String> {
    let mut frames: Vec<String> = Vec::new();
    for line in backtrace.lines().map(str::trim) {
        if let Some((index, symbol)) = line.split_once(": ") {
            if !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) {
                frames.push(symbol.to_owned());
                continue;
            }
        }
        if let (Some(place), Some(last)) = (line.strip_prefix("at "), frames.last_mut()) {
            last.push_str(" at ");
            last.push_str(place);
        }
    }
    frames
}
