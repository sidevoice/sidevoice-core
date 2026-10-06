//! The single owner of private files and process locks.

mod legacy;

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde_json::Value;
use tempfile::NamedTempFile;

#[derive(Clone, Debug)]
pub struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    /// Create `path` if needed and accept it only as an owner-only directory of this user.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let found = fs::symlink_metadata(path)?;
        if !is_own_directory(&found) || found.permissions().mode() & 0o077 != 0 {
            return Err(unsafe_directory());
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    /// Accept an existing directory of this user, whatever its mode, to leave a failure report in.
    pub fn open_for_report(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if !is_own_directory(&fs::symlink_metadata(path)?) {
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

    pub fn read_json(&self, name: &str) -> io::Result<Option<Value>> {
        match fs::read(self.file(name)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
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
        File::open(&self.path)?.sync_all()
    }

    pub fn link_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let staged = self.stage(bytes)?;
        fs::hard_link(staged.path(), self.file(name))?;
        File::open(&self.path)?.sync_all()
    }

    fn stage(&self, bytes: &[u8]) -> io::Result<NamedTempFile> {
        let mut staged = NamedTempFile::new_in(&self.path)?;
        staged
            .as_file()
            .set_permissions(Permissions::from_mode(0o600))?;
        staged.write_all(bytes)?;
        staged.as_file().sync_all()?;
        Ok(staged)
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.file(name)) {
            Ok(()) => File::open(&self.path)?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn lock(&self) -> io::Result<Option<File>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.file("core.lock"))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn is_own_directory(found: &fs::Metadata) -> bool {
    found.file_type().is_dir() && found.uid() == rustix::process::getuid().as_raw()
}

fn unsafe_directory() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "identity.unsafe-directory")
}

#[cfg(test)]
mod tests;
