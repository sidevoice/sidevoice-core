//! The private, size-rotated process log: the `core.log` file the subscriber writes through.

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

use crate::platform::fs::{Create, Files, Fs};
use crate::storage::PrivateDir;

const ROTATE_AT_BYTES: u64 = 5_000_000;

thread_local! {
    /// Set while the panic hook logs: a panic may come from inside a write that holds the lock, which this thread
    /// then must not wait for.
    pub(super) static IN_PANIC_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// The log file. Every line is judged again before it is written: a log that became a link, or open to others, is
/// refused, not repaired, and the line is dropped.
pub(super) struct LogFile {
    path: PathBuf,
    /// The spellings of the home directory, each shown as `~` (the writer backstop of `docs/logging.md`, 8.5).
    homes: Vec<String>,
    lock: Mutex<()>,
}

impl LogFile {
    /// Opens (creating, judging and rotating) the log now: a log that cannot be opened is a start failure, which a
    /// writer made lazily by the subscriber could not report.
    pub(super) fn open(path: PathBuf, home: Option<&Path>) -> io::Result<Self> {
        let mut homes = Vec::new();
        // The root, or an empty home, would blank every path out.
        if let Some(home) = home.and_then(Path::to_str).filter(|home| home.len() > 1) {
            let escaped = format!("{home:?}");
            for spelling in [
                home.to_owned(),
                escaped[1..escaped.len() - 1].to_owned(),
                home.replace('\\', "/"),
            ] {
                if !homes.contains(&spelling) {
                    homes.push(spelling);
                }
            }
        }
        let log = Self {
            path,
            homes,
            lock: Mutex::new(()),
        };
        log.append(b"")?;
        Ok(log)
    }

    fn append(&self, line: &[u8]) -> io::Result<()> {
        let _held = (!IN_PANIC_HOOK.get())
            .then(|| self.lock.lock().unwrap_or_else(PoisonError::into_inner));
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            PrivateDir::open(parent)?;
        }
        // Judged before anything else, whatever its size: a full log that is a link or open to others is refused,
        // not rotated out of the way.
        let mut file = open_private_append(&self.path)?;
        if file.metadata()?.len() >= ROTATE_AT_BYTES {
            drop(file);
            rotate_if_full(&self.path)?;
            file = open_private_append(&self.path)?;
        }
        let mut text = String::from_utf8_lossy(line).into_owned();
        for home in &self.homes {
            text = text.replace(home.as_str(), "~");
        }
        file.write_all(text.as_bytes())
    }
}

/// One event's line: the formatter writes each event with one `write`.
pub(super) struct Line<'a>(&'a LogFile);

impl Write for Line<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.append(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = Line<'a>;

    fn make_writer(&'a self) -> Line<'a> {
        Line(self)
    }
}

/// Keep two older generations, `<log>.1` and `<log>.2`, once the log reaches its size limit.
pub(super) fn rotate_if_full(path: &Path) -> io::Result<()> {
    if !fs::metadata(path).is_ok_and(|meta| meta.len() >= ROTATE_AT_BYTES) {
        return Ok(());
    }
    let first = path.with_extension("log.1");
    let second = path.with_extension("log.2");
    let _ = fs::remove_file(&second);
    if first.exists() {
        fs::rename(&first, &second)?;
    }
    fs::rename(path, &first)
}

/// The log, opened for appending: created private, a link in its place refused, and a file already there that others
/// can open (or that is not this user's regular file) refused rather than repaired.
fn open_private_append(path: &Path) -> io::Result<File> {
    let file = Files::open_private(path, Create::Append)?;
    Files::check_private(&file)?;
    Ok(file)
}
