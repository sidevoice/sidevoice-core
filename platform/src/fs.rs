//! Private directories and files: the file-system half of the trust boundary, the OS user. What keeps another user
//! out is that our own directories and files are open to nobody else and that a link planted in their place is never
//! followed. Anything that fails a check is refused, never repaired behind the person's back and never used.
//!
//! The interface is the [`Fs`] trait; [`Files`] is the one OS implementation compiled in, chosen below. What differs
//! per OS is only how access is set and read: Unix mode bits and `O_NOFOLLOW`; on Windows a protected DACL set when
//! a file or directory is created, read back from the handle, and reparse points (links, junctions) refused. Who owns
//! something is [`user`](crate::user)'s (`UserId::current().owns_path/owns_file`). Callers import both:
//! `use sidevoice_platform::fs::{Files, Fs};` then `Files::create_private_dirs(..)`, `Files::open_private(..)`.

#[cfg(unix)]
#[path = "fs/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "fs/windows.rs"]
mod imp;

pub use imp::Os as Files;

use crate::user::{User, UserId};
use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::Path;

/// How [`Fs::open_private`] opens a file of ours. Whatever it creates is private; a link in its place is refused,
/// never followed.
#[derive(Clone, Copy, Debug)]
pub enum Create {
    /// For appending, created when missing.
    Append,
    /// For reading and writing, created when missing, what is in it kept.
    ReadWrite,
}

/// What every OS implements ([`Files`], in `fs/unix.rs` and `fs/windows.rs`): the required methods. The rest is one
/// body for every OS; an OS overrides it only if it must.
pub trait Fs {
    /// Makes one directory, private, failing when something is already there.
    fn create_private_dir(path: &Path) -> io::Result<()>;

    /// Opens a file of ours as `how` says. A file already there is opened as it is, its access untouched.
    fn open_private(path: &Path, how: Create) -> io::Result<File>;

    /// Opens `path` (a file or a directory) for reading: a link there is refused, not followed, and nothing is
    /// waited on (a FIFO or a device put in a file's place).
    fn open_read(path: &Path) -> io::Result<File>;

    /// Whether anybody but the owner (and the system: root, SYSTEM, Administrators) may do anything with the file or
    /// directory behind an open handle.
    fn open_to_others(file: &File) -> io::Result<bool>;

    /// Makes what was just created, renamed or removed in the directory `dir` durable, where the OS has a way: Unix
    /// syncs the directory; Windows does nothing (NTFS journals its metadata and gives no handle to flush).
    fn sync_dir(dir: &Path) -> io::Result<()>;

    /// Makes `path` and any missing parents, each private; what is already there is left as it is.
    fn create_private_dirs(path: &Path) -> io::Result<()> {
        let created = match Self::create_private_dir(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    Self::create_private_dirs(parent)?;
                }
                Self::create_private_dir(path)
            }
            other => other,
        };
        match created {
            Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
            other => other,
        }
    }

    /// Whether `path` is a directory itself (a link to one is not), this user's, and open to nobody else. A missing
    /// path is an error; a link, or anything the checks cannot read, is not private.
    fn is_private_dir(path: &Path) -> io::Result<bool> {
        let dir = match Self::open_read(path) {
            Ok(dir) => dir,
            Err(error) if error.kind() == ErrorKind::NotFound => return Err(error),
            Err(_) => return Ok(false),
        };
        Ok(dir.metadata()?.is_dir()
            && UserId::current().owns_file(&dir)?
            && !Self::open_to_others(&dir)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn private(path: &Path) -> bool {
        Files::is_private_dir(path).unwrap()
    }

    #[test]
    fn a_directory_it_makes_is_this_user_s_alone_with_its_missing_parents() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a").join("b");
        Files::create_private_dirs(&dir).unwrap();
        Files::create_private_dirs(&dir).unwrap();
        assert!(private(&dir));
        assert!(private(dir.parent().unwrap()), "each parent it made");
        assert!(UserId::current().owns_path(&dir).unwrap());
        assert!(!Files::open_to_others(&Files::open_read(&dir).unwrap()).unwrap());
    }

    #[test]
    fn a_directory_others_can_enter_is_not_private_and_is_left_as_it_is() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data");
        Files::create_private_dirs(&dir).unwrap();
        imp::let_others_in(&dir);
        assert!(Files::open_to_others(&Files::open_read(&dir).unwrap()).unwrap());
        assert!(!private(&dir));
        Files::create_private_dirs(&dir).unwrap();
        assert!(!private(&dir), "making it again does not repair it");
    }

    #[test]
    fn a_file_a_missing_path_and_a_link_are_not_private_directories() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(!private(&file));
        assert!(Files::is_private_dir(&root.path().join("missing")).is_err());
        let target = root.path().join("target");
        Files::create_private_dirs(&target).unwrap();
        let link = root.path().join("link");
        imp::link_dir(&target, &link);
        assert!(private(&target));
        assert!(!private(&link), "a link is not the directory it leads to");
    }

    #[test]
    fn the_private_openers_make_private_files_and_keep_what_is_in_them() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("log");
        writeln!(Files::open_private(&log, Create::Append).unwrap(), "one").unwrap();
        writeln!(Files::open_private(&log, Create::Append).unwrap(), "two").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        let held = Files::open_private(&log, Create::ReadWrite).unwrap();
        assert!(!Files::open_to_others(&held).unwrap());
        assert!(UserId::current().owns_file(&held).unwrap());
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n", "kept");
    }

    #[test]
    fn a_link_in_the_place_of_a_private_file_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("somebody-else");
        std::fs::write(&target, b"untouched").unwrap();
        let link = root.path().join("log");
        imp::link_file(&target, &link);
        assert!(Files::open_private(&link, Create::Append).is_err());
        assert!(Files::open_private(&link, Create::ReadWrite).is_err());
        assert!(Files::open_read(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
    }

    #[test]
    fn a_directory_can_be_synced() {
        let root = tempfile::tempdir().unwrap();
        Files::sync_dir(root.path()).unwrap();
    }
}
