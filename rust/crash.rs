//! Crash records: what a panic of this machine's pieces leaves behind for a paired app to read.
//!
//! The machine only stores. It never sends a record anywhere and has no setting about them: a paired app lists the
//! records over the device surface (`GET /api/device/crashes`), decides what to do with them, and acknowledges each one
//! it has handled (`DELETE /api/device/crashes/{id}`), which deletes it. Nothing on the connector link and nothing the
//! agent reaches can list, change or acknowledge a record (`rust/server/crashes.rs`).
//!
//! Ownership. The core owns the store (`D/core/crashes`, one file per record, written only by the core) and its caps.
//! The connector never writes there: it drops its own record, once and complete, in a spool directory beside the core's
//! (`D/crash-inbox`, [`INBOX`]), and the core takes each file in when it next reads the store, validating it again,
//! applying the caps and deleting the file. Each file has one writer at a time.
//!
//! A record holds a closed list of fields ([`CrashRecord`]) and nothing else; text in it is bounded and free of control
//! characters. The panic message is only ever a `&'static str` literal or the fixed marker: the formatted message of
//! `docs/logging.md` section 9 is never stored.

mod record;
mod store;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use record::{CrashRecord, Exception, Location, Piece};
pub use store::CrashStore;

/// The core's store, inside the core's own directory.
const STORE: &str = "crashes";
/// The spool the connector writes into, beside the core's directory (`D/crash-inbox`); the same name is in the
/// connector, which cannot depend on this crate.
pub const INBOX: &str = "crash-inbox";

/// The most records kept.
pub const MAX_RECORDS: usize = 20;
/// The most bytes the records take together.
pub const MAX_BYTES: u64 = 1024 * 1024;
/// The longest a record is kept, from its last occurrence.
pub const MAX_AGE_DAYS: i64 = 30;
/// The most bytes one record file may have: a bigger one is not ours and is deleted.
pub const MAX_RECORD_BYTES: u64 = 64 * 1024;
/// The most lines of the piece's own log a record carries.
pub const TAIL_LINES: usize = 50;
/// The most frames of the backtrace a record carries.
pub const MAX_FRAMES: usize = 64;

/// Where the store and the spool are for the core whose directory is `data` (`D/core`).
fn locations(data: &Path) -> (PathBuf, PathBuf) {
    let inbox = data.parent().unwrap_or_else(|| Path::new(".")).join(INBOX);
    (data.join(STORE), inbox)
}

/// What the panic hook needs and cannot ask for while the process is panicking.
struct Context {
    store: CrashStore,
    log_file: PathBuf,
    homes: Vec<String>,
    os: &'static str,
    arch: &'static str,
}

static CONTEXT: OnceLock<Context> = OnceLock::new();

/// From now on a panic of this process leaves a record. `homes` are the spellings of the home directory, each shown as
/// `~` (the log's own backstop, reused here).
pub fn install(
    data: &Path,
    log_file: PathBuf,
    homes: Vec<String>,
    os: &'static str,
    arch: &'static str,
) {
    let _ = CONTEXT.set(Context {
        store: CrashStore::new(data),
        log_file,
        homes,
        os,
        arch,
    });
}

/// What a panic is, as the hook knows it.
pub struct Panic<'a> {
    /// The payload when it is a `&'static str`; `None` for anything formatted.
    pub literal: Option<&'a str>,
    pub file: &'a str,
    pub line: u32,
    /// `Backtrace`'s `Display`.
    pub backtrace: &'a str,
}

/// Stores the record of a panic. Best effort and never panics: the hook that calls it is already panicking, and a record
/// that cannot be written must not hide the log's own record of the panic.
pub fn capture(panic: &Panic<'_>) {
    let Some(context) = CONTEXT.get() else {
        return;
    };
    let tail = store::log_tail(&context.log_file);
    let record = CrashRecord::from_panic(
        Piece::Core,
        env!("CARGO_PKG_VERSION"),
        context.os,
        context.arch,
        panic,
        &tail,
        &context.homes,
    );
    // A thread that panicked inside the store holds its lock: do not wait for it.
    context.store.record_without_waiting(record);
}
