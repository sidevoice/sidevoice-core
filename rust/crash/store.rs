//! The bounded store: one private file per record, coalesced by signature, evicted by the caps, tolerant of anything
//! it finds that is not a record.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::record::CrashRecord;
use super::{locations, MAX_AGE_DAYS, MAX_BYTES, MAX_RECORDS, MAX_RECORD_BYTES, TAIL_LINES};
use crate::platform::fs::{Files, Fs};

/// The one lock of the store in this process (the core holds the data directory's lock, so there is no other process).
static LOCK: Mutex<()> = Mutex::new(());
/// The most spool files taken in by one read; the rest are deleted unread (a flood is not a set of crashes).
const MAX_INBOX_FILES: usize = 100;
/// How much of the end of a log is read to find its last lines.
const TAIL_BYTES: u64 = 64 * 1024;

pub struct CrashStore {
    dir: PathBuf,
    inbox: PathBuf,
}

/// A record on disk and the bytes it takes.
struct Kept {
    record: CrashRecord,
    bytes: u64,
}

impl CrashStore {
    /// The store of the core whose private directory is `data`.
    pub fn new(data: &Path) -> Self {
        let (dir, inbox) = locations(data);
        Self { dir, inbox }
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("crash-{id}.json"))
    }

    /// Keeps `record`: merged into the one with its signature if there is one, then the caps are applied.
    pub fn record(&self, record: CrashRecord) -> io::Result<()> {
        let _held = lock();
        self.add(record)
    }

    /// As [`CrashStore::record`] for the panic hook: a lock somebody else holds means this record is dropped, never
    /// waited for. Nothing is returned: there is nobody to tell.
    pub fn record_without_waiting(&self, record: CrashRecord) {
        let _held = match LOCK.try_lock() {
            Ok(held) => held,
            Err(TryLockError::Poisoned(held)) => held.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        let _ = self.add(record);
    }

    /// The records, newest first: the spool is taken in first, and what is expired or unreadable is deleted.
    pub fn list(&self) -> io::Result<Vec<CrashRecord>> {
        let _held = lock();
        self.take_inbox();
        let mut kept = self.load()?;
        self.enforce(&mut kept);
        kept.sort_by(|a, b| b.record.last_at.cmp(&a.record.last_at));
        Ok(kept.into_iter().map(|kept| kept.record).collect())
    }

    /// Deletes the record `id`: the app has handled it. `false` when there is none.
    pub fn acknowledge(&self, id: &str) -> io::Result<bool> {
        // Only a UUID names a file: nothing else reaches the file system.
        if Uuid::parse_str(id).map(|uuid| uuid.to_string()).as_deref() != Ok(id) {
            return Ok(false);
        }
        let _held = lock();
        let path = self.file(id);
        match fs::remove_file(&path) {
            Ok(()) => {
                let _ = Files::sync_dir(&self.dir);
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn add(&self, mut record: CrashRecord) -> io::Result<()> {
        Files::ensure_private_dir(&self.dir)?;
        let mut kept = self.load()?;
        let same = kept
            .iter()
            .position(|kept| kept.record.signature == record.signature);
        match same {
            Some(index) => {
                let found = &mut kept[index];
                found.record.count = found.record.count.saturating_add(record.count);
                if record.last_at > found.record.last_at {
                    found.record.last_at = record.last_at;
                }
                if record.first_at < found.record.first_at {
                    found.record.first_at = record.first_at;
                }
                let bytes = serde_json::to_vec(&found.record)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                Files::write_private(&self.file(&found.record.id), &bytes)?;
                found.bytes = bytes.len() as u64;
            }
            None => {
                let bytes = serde_json::to_vec(&record)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if bytes.len() as u64 > MAX_RECORD_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "too large"));
                }
                // A new record never replaces a file: an id that exists is not made again.
                record.id = record.id.to_lowercase();
                Files::create_private(&self.file(&record.id), &bytes)?;
                kept.push(Kept {
                    record,
                    bytes: bytes.len() as u64,
                });
            }
        }
        self.enforce(&mut kept);
        Ok(())
    }

    /// Every valid record on disk. What is a record's name but is not one (too big, not JSON, not the closed shape, not
    /// named by its own id) is deleted; what is not named like a record is left alone.
    fn load(&self) -> io::Result<Vec<Kept>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut kept = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(id) = name
                .strip_prefix("crash-")
                .and_then(|rest| rest.strip_suffix(".json"))
            else {
                continue;
            };
            match read(&entry.path()).filter(|(record, _)| record.id == id) {
                Some((record, bytes)) => kept.push(Kept { record, bytes }),
                None => {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(kept)
    }

    /// Deletes what is past the age, then the oldest until the count and the size fit.
    fn enforce(&self, kept: &mut Vec<Kept>) {
        let limit = Utc::now() - Duration::days(MAX_AGE_DAYS);
        let expired = |kept: &Kept| {
            DateTime::parse_from_rfc3339(&kept.record.last_at).map_or(true, |at| at < limit)
        };
        let remove = |kept: &Kept| {
            let _ = fs::remove_file(self.file(&kept.record.id));
        };
        kept.retain(|item| {
            let old = expired(item);
            if old {
                remove(item);
            }
            !old
        });
        kept.sort_by(|a, b| a.record.last_at.cmp(&b.record.last_at));
        while kept.len() > MAX_RECORDS || kept.iter().map(|k| k.bytes).sum::<u64>() > MAX_BYTES {
            let oldest = kept.remove(0);
            remove(&oldest);
        }
        let _ = Files::sync_dir(&self.dir);
    }

    /// Takes in the records the connector left in the spool. A file that is not a valid record is deleted too.
    fn take_inbox(&self) {
        let Ok(entries) = fs::read_dir(&self.inbox) else {
            return;
        };
        for (taken, entry) in entries.flatten().enumerate() {
            let path = entry.path();
            let named = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("crash-") && name.ends_with(".json"));
            if !named {
                continue;
            }
            if taken < MAX_INBOX_FILES {
                if let Some((record, _)) = read(&path) {
                    if self.add(record).is_err() {
                        // Left in the spool for the next read.
                        continue;
                    }
                }
            }
            let _ = fs::remove_file(&path);
        }
    }
}

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The record in `path` and its size, `None` when it is not one: not a private regular file of ours (as the log is
/// judged), over the size, not JSON, or not the closed shape.
fn read(path: &Path) -> Option<(CrashRecord, u64)> {
    let file = Files::open_private_file(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return None;
    }
    let value = serde_json::from_slice(&bytes).ok()?;
    CrashRecord::from_untrusted(&value).map(|record| (record, bytes.len() as u64))
}

/// The last lines of `log`, at most [`TAIL_LINES`], without the backtrace events (the record has the frames). Empty when
/// the log cannot be read: a record without a tail is still a record.
pub(super) fn log_tail(log: &Path) -> Vec<String> {
    let read = || -> io::Result<Vec<String>> {
        let mut file = Files::open_private_file(log)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))?;
        let mut bytes = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let mut lines: Vec<&str> = text.lines().collect();
        if length > TAIL_BYTES && !lines.is_empty() {
            // The first line may be cut in the middle.
            lines.remove(0);
        }
        let lines: Vec<String> = lines
            .into_iter()
            .filter(|line| !line.contains("crash_backtrace"))
            .map(str::to_owned)
            .collect();
        let skip = lines.len().saturating_sub(TAIL_LINES);
        Ok(lines.into_iter().skip(skip).collect())
    };
    read().unwrap_or_default()
}
