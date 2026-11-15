//! The bounded store: one private file per record, coalesced by signature, evicted by the caps, tolerant of anything
//! it finds that is not a record.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration as Age, SystemTime};

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use super::record::CrashRecord;
use super::{locations, MAX_AGE_DAYS, MAX_BYTES, MAX_RECORDS, MAX_RECORD_BYTES};
use crate::platform::fs::{Files, Fs};

/// The one lock of the store in this process (the core holds the data directory's lock, so there is no other process).
static LOCK: Mutex<()> = Mutex::new(());
/// The most spool files taken in by one read; the rest are deleted unread (a flood is not a set of crashes).
const MAX_INBOX_FILES: usize = 100;
/// How much of the end of a log is read to find its last lines.
const TAIL_BYTES: u64 = 64 * 1024;
/// How old a staging file in the spool is before it is taken for a writer that died.
const STAGING_AGE: Age = Age::from_secs(60 * 60);

pub struct CrashStore {
    dir: PathBuf,
    inbox: PathBuf,
    /// Every spelling of the home directory, hidden again in what comes from the spool.
    homes: Vec<String>,
}

/// A record on disk and the bytes it takes.
struct Kept {
    record: CrashRecord,
    bytes: u64,
}

impl CrashStore {
    /// The store of the core whose private directory is `data`.
    pub fn new(data: &Path, homes: Vec<String>) -> Self {
        let (dir, inbox) = locations(data);
        Self { dir, inbox, homes }
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

    /// Deletes the record `id`: the app has handled it. Its signature, or `None` when there is no such record.
    pub fn acknowledge(&self, id: &str) -> io::Result<Option<String>> {
        // Only a UUID names a file: nothing else reaches the file system.
        if Uuid::parse_str(id).map(|uuid| uuid.to_string()).as_deref() != Ok(id) {
            return Ok(None);
        }
        let _held = lock();
        let path = self.file(id);
        let signature = read(&path, &self.homes).map(|(record, _)| record.signature);
        match fs::remove_file(&path) {
            Ok(()) => {
                let _ = Files::sync_dir(&self.dir);
                Ok(Some(signature.unwrap_or_default()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
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
            match read(&entry.path(), &self.homes).filter(|(record, _)| record.id == id) {
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

    /// Takes in the records the connector left in the spool, and applies the caps. Done when the core starts as well as
    /// on every read, so a spool nobody reads is not left to grow.
    pub fn maintain(&self) {
        let _ = self.list();
    }

    /// Takes in the records the connector left in the spool. A file that is not a valid record, that is dated in the
    /// future, or that the store refuses for a reason that will not change (an id it has, too big) is deleted; one it
    /// could not take in for a reason that may pass (a disk error) stays for the next read. A staging file a writer
    /// never finished is deleted once it is old; one that may still be written is not looked at.
    fn take_inbox(&self) {
        // A link in the spool's place is never followed: it is not a directory of ours.
        if fs::symlink_metadata(&self.inbox).is_ok_and(|meta| meta.file_type().is_symlink())
            || (self.inbox.exists() && Files::ensure_private_dir(&self.inbox).is_err())
        {
            return;
        }
        let Ok(entries) = fs::read_dir(&self.inbox) else {
            return;
        };
        let stale = SystemTime::now() - STAGING_AGE;
        let mut taken = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".crash-") {
                let old = fs::symlink_metadata(&path)
                    .and_then(|meta| meta.modified())
                    .is_ok_and(|modified| modified < stale);
                if old {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }
            if !(name.starts_with("crash-") && name.ends_with(".json")) {
                continue;
            }
            taken += 1;
            if taken <= MAX_INBOX_FILES {
                if let Some((record, _)) = read(&path, &self.homes) {
                    if !record.is_dated_in_the_future() {
                        match self.add(record) {
                            Ok(()) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::AlreadyExists | io::ErrorKind::InvalidData
                                ) => {}
                            // Left in the spool for the next read.
                            Err(_) => continue,
                        }
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
fn read(path: &Path, homes: &[String]) -> Option<(CrashRecord, u64)> {
    let file = Files::open_created_file(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return None;
    }
    let value = serde_json::from_slice(&bytes).ok()?;
    CrashRecord::from_untrusted(&value, homes).map(|record| (record, bytes.len() as u64))
}

/// The lines in the last [`TAIL_BYTES`] of `log`, without the backtrace events (the record has the frames); the record keeps
/// the last of those that are ours ([`CrashRecord::from_panic`]). Empty when the log cannot be read: a record without a
/// tail is still a record.
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
        Ok(lines
            .into_iter()
            .filter(|line| !line.contains("crash_backtrace"))
            .map(str::to_owned)
            .collect())
    };
    read().unwrap_or_default()
}
