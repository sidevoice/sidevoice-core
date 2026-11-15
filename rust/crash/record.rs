//! The record's closed field list, its signature and the validation every record passes before it is kept.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{Panic, MAX_FRAMES, TAIL_LINES};

/// How far ahead of this machine's clock a record may be dated: a clock a little ahead is not a forgery.
const SKEW_MINUTES: i64 = 5;

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
        let hide = |text: &str| mask_homes(text, homes);
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
    /// the signature is made again rather than believed. The home directory is hidden again and the times are made UTC
    /// (milliseconds) and ordered. `None` when a required field is missing or malformed.
    pub fn from_untrusted(value: &Value, homes: &[String]) -> Option<Self> {
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
        let times = |text: &str| {
            chrono::DateTime::parse_from_rfc3339(text)
                .ok()
                .map(|at| at.with_timezone(&chrono::Utc))
        };
        let (first, last) = (times(&record.first_at)?, times(&record.last_at)?);
        let stamp = |at: chrono::DateTime<chrono::Utc>| {
            at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        };
        record.first_at = stamp(first.min(last));
        record.last_at = stamp(first.max(last));
        record.exception.message = mask_homes(&record.exception.message, homes);
        record.location.file = mask_homes(&record.location.file, homes);
        for text in record.backtrace.iter_mut().chain(&mut record.log_tail) {
            *text = mask_homes(text, homes);
        }
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
        {
            return None;
        }
        record.normalize();
        Some(record)
    }

    /// Whether the record claims to have happened later than a clock a few minutes ahead allows. Both times are
    /// canonical UTC by now, so they compare as texts.
    pub fn is_dated_in_the_future(&self) -> bool {
        let limit = (chrono::Utc::now() + chrono::Duration::minutes(SKEW_MINUTES))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.first_at > limit || self.last_at > limit
    }

    /// Bounds every text, keeps only what has the shape of what we write, and makes the signature: the message is
    /// printable ASCII or the marker, the file a short relative path, a frame `symbol` or `symbol at path:line`, a log
    /// line one of our own warnings or errors.
    fn normalize(&mut self) {
        let message = clean(&self.exception.message, MAX_TEXT);
        self.exception.message = if message.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            message
        } else {
            FORMATTED_MARKER.to_owned()
        };
        let file = clean(&self.location.file, MAX_TEXT).replace('\\', "/");
        let relative = !file.starts_with('/')
            && file.split('/').all(|part| part != ".." && !part.is_empty())
            && file
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c));
        self.location.file = if relative { file } else { "unknown".to_owned() };
        self.backtrace = self
            .backtrace
            .iter()
            .map(|frame| clean(frame, MAX_FRAME))
            .filter(|frame| is_frame(frame))
            .take(MAX_FRAMES)
            .collect();
        let piece = self.piece;
        let lines: Vec<String> = self
            .log_tail
            .iter()
            .map(|line| clean(line, MAX_LINE))
            .filter(|line| is_ours(line, piece))
            .collect();
        let skip = lines.len().saturating_sub(TAIL_LINES);
        self.log_tail = lines.into_iter().skip(skip).collect();
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

/// `text` with control characters as spaces and invisible format characters (bidi controls among them) removed, cut to
/// at most `max` characters.
fn clean(text: &str, max: usize) -> String {
    text.chars()
        .filter(|c| !is_format(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect()
}

/// Unicode format characters (category Cf): they change how a text is shown without being seen.
fn is_format(c: char) -> bool {
    matches!(c,
        '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}' | '\u{180e}'
        | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}'
        | '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e007f}')
}

/// `text` with every spelling of the home directory shown as `~` (on Windows without regard to case).
pub(super) fn mask_homes(text: &str, homes: &[String]) -> String {
    homes
        .iter()
        .filter(|home| home.len() > 1)
        .fold(text.to_owned(), |text, home| {
            if !cfg!(windows) {
                return text.replace(home.as_str(), "~");
            }
            // ASCII lowercasing keeps every byte offset.
            let (folded, needle) = (text.to_ascii_lowercase(), home.to_ascii_lowercase());
            let mut out = String::with_capacity(text.len());
            let mut from = 0;
            while let Some(at) = folded[from..].find(&needle) {
                out.push_str(&text[from..from + at]);
                out.push('~');
                from += at + needle.len();
            }
            out.push_str(&text[from..]);
            out
        })
}

/// A frame as [`frames`] makes it: a symbol, then optionally ` at <path>:<line>[:<column>]`.
fn is_frame(frame: &str) -> bool {
    let (symbol, place) = match frame.rsplit_once(" at ") {
        Some((symbol, place)) => (symbol, Some(place)),
        None => (frame, None),
    };
    fn number(text: &str) -> Option<&str> {
        text.rsplit_once(':')
            .filter(|(_, n)| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
            .map(|(rest, _)| rest)
    }
    let place_ok = place.is_none_or(|place| {
        number(place).is_some_and(|path| {
            let path = number(path).unwrap_or(path);
            !path.is_empty()
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_./\\~:+-".contains(c))
        })
    });
    !symbol.is_empty()
        && symbol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_:<>{}$.,&*()[]'+-@# ".contains(c))
        && place_ok
}

/// Whether `line` is a line of our own log at `warn` or `error`: `<RFC 3339 time> <LEVEL> <target>: ...`, the target
/// one of this piece's own crates. A dependency's line, a debug line and a service's raw stderr are none of these.
fn is_ours(line: &str, piece: Piece) -> bool {
    let mut parts = line.split_whitespace();
    let (Some(time), Some(level)) = (parts.next(), parts.next()) else {
        return false;
    };
    if chrono::DateTime::parse_from_rfc3339(time).is_err() || !matches!(level, "WARN" | "ERROR") {
        return false;
    }
    let rest = line.trim_start()[time.len()..].trim_start();
    let rest = rest[level.len()..].trim_start();
    let Some((target, _)) = rest.split_once(": ") else {
        return false;
    };
    let own = match piece {
        Piece::Core => "sidevoice_core",
        Piece::Connector => "sidevoice_connector",
    };
    [own, "sidevoice_platform", "panic"].iter().any(|name| {
        target == *name
            || target
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with("::"))
    })
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
