//! The single owner of private files and process locks.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::NamedTempFile;

use crate::platform::fs::{Create, Files, Fs};
use crate::platform::user::{User, UserId};

#[derive(Clone, Debug)]
pub struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    /// Create `path` if needed and accept it only as an owner-only directory of this user whose every ancestor is this
    /// user's or the system's with nobody else able to replace what is in it (`Files::ensure_private_dir`); a refusal is
    /// `identity.unsafe-directory`.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        Files::ensure_private_dir(path).map_err(|error| match error.kind() {
            io::ErrorKind::PermissionDenied => unsafe_directory(),
            _ => error,
        })?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Accept an existing directory of this user, whatever its mode, to leave a failure report in.
    pub fn open_for_report(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if !is_own_directory(path, &fs::symlink_metadata(path)?) {
            return Err(unsafe_directory());
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// A record of ours, `None` when there is none. It is read from the handle that was judged: opened without
    /// following a link, and used only when it is a regular file of this user's with one name, open to nobody else
    /// (`Files::open_private_file`); anything else is refused with `PermissionDenied` and left as it is. What does not
    /// parse is `InvalidData`.
    pub fn read_json(&self, name: &str) -> io::Result<Option<Value>> {
        let mut file = match Files::open_private_file(&self.file(name)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub fn write_json(&self, name: &str, value: &Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.write_private(name, &bytes)
    }

    fn write_private(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut staged = self.stage(bytes)?;
        staged.flush()?;
        staged
            .persist(self.file(name))
            .map_err(|error| error.error)?;
        Files::sync_dir(&self.path)
    }

    pub fn link_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let staged = self.stage(bytes)?;
        fs::hard_link(staged.path(), self.file(name))?;
        Files::sync_dir(&self.path)
    }

    /// A temporary file in the private directory, created private by `Files::open_private` (`0600` on Unix, its own
    /// protected owner, SYSTEM and Administrators DACL on Windows, nothing taken from the directory) before any byte
    /// is written, under a fresh name `tempfile` picks and removes on failure.
    fn stage(&self, bytes: &[u8]) -> io::Result<NamedTempFile<File>> {
        let mut staged = tempfile::Builder::new()
            .make_in(&self.path, |path| Files::open_private(path, Create::New))?;
        staged.write_all(bytes)?;
        staged.as_file().sync_all()?;
        Ok(staged)
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.file(name)) {
            Ok(()) => Files::sync_dir(&self.path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Takes the directory's lock, `None` while another process holds it. A lock file already there is used only when
    /// it is a regular file of this user's with one name, open to nobody else (`Files::check_private`; a directory there
    /// cannot be opened for writing); anything else is refused and left as it is.
    pub fn lock(&self) -> io::Result<Option<File>> {
        let file = Files::open_private(&self.file("core.lock"), Create::ReadWrite)?;
        Files::check_private(&file)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

fn is_own_directory(path: &Path, found: &fs::Metadata) -> bool {
    found.file_type().is_dir() && UserId::current().owns_path(path).unwrap_or(false)
}

fn unsafe_directory() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "identity.unsafe-directory")
}

/// Whether the file at `path` is a regular file of this user's with one name, open to nobody else, judged on a handle
/// opened without following a link (tests, on every OS).
#[cfg(test)]
pub(crate) fn is_private_file(path: &Path) -> bool {
    Files::open_private_file(path).is_ok()
}

/// A temporary directory that is private to this user: `tempfile` makes its own with the process's umask, and the core refuses
/// a log whose parent is not private. `path()` is the private directory inside it (tests, on every OS).
#[cfg(test)]
pub(crate) struct PrivateTemp {
    _outer: tempfile::TempDir,
    dir: PathBuf,
}

#[cfg(test)]
impl PrivateTemp {
    pub(crate) fn new() -> Self {
        let outer = tempfile::tempdir().unwrap();
        let dir = outer.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        Self { _outer: outer, dir }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests;
