//! Private directories and files: the file-system half of the trust boundary, the OS user. What keeps another user
//! out is that every directory on the way to our files is this user's (or the system's) and nobody else can replace
//! what is in it, that our own directories and records are open to nobody else and have no second name, and that a
//! link planted in their place is never followed. Anything that fails a check is refused (`PermissionDenied`), never
//! repaired behind the person's back and never used.
//!
//! The interface is the [`Fs`] trait; [`Files`] is the one OS implementation compiled in, chosen below. What differs
//! per OS is only how access is set and read: Unix mode bits and `O_NOFOLLOW`; on Windows a protected DACL set when a
//! file or directory is created, read back from the handle (one that is not protected, or that passes access down to
//! others, is not private), and reparse points (links, junctions, mount points) refused, at the path and on the way to
//! it. Who owns something is [`user`](crate::user)'s (`UserId::current().owns_path/owns_file`). Everything else has one
//! body for every OS, the trait's default methods. Records are written whole: a temporary file beside the record
//! (`.<record>.<random>`, the name `tempfile` picks), private from its creation, synced, renamed over it (or, for a
//! record made once, given its name with nothing replaced), the directory synced; on Unix every step in the directory
//! is taken relative to one handle open on it. Callers import both: `use sidevoice_platform::fs::{Files, Fs};` then
//! `Files::ensure_private_dir(..)`, `Files::read_trusted(..)`.

#[cfg(unix)]
#[path = "fs/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "fs/windows.rs"]
mod imp;

pub use imp::Os as Files;

use crate::user::{User, UserId};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

/// What somebody other than a file's owner (and the system: root, SYSTEM, Administrators) may do with it.
#[derive(Debug, PartialEq, Eq)]
pub enum Others {
    /// Nothing at all, now or through what is created in it: the file is private.
    Nothing,
    /// Read it, never change it; or (Windows) nothing yet, but its DACL is not protected (it takes what its parent
    /// passes down), or a directory passes access down to what is created in it.
    Read,
    /// Change it, or (a directory) replace what is in it.
    Change,
}

/// How [`Fs::open_private`] opens a file of ours. Whatever it creates is private; a link in its place is refused,
/// never followed. A file already there is opened as it is, its access untouched.
#[derive(Clone, Copy, Debug)]
pub enum Create {
    /// A new file for writing, never one already there.
    New,
    /// As `New`, for a program: executable where the OS marks that on the file (Unix).
    NewProgram,
    /// For appending, created when missing.
    Append,
    /// For reading and writing, created when missing, what is in it kept.
    ReadWrite,
}

/// Which file a handle is open on ([`Fs::identity`]): equal for two handles to one file, different for a file that
/// took its path (a rotation, a replacement). Compared only with another reading on this machine; holds nothing open.
#[derive(PartialEq)]
pub struct FileId {
    volume: u64,
    index: u128,
}

/// What every OS implements ([`Files`], in `fs/unix.rs` and `fs/windows.rs`): the required methods. The rest is one
/// body for every OS; an OS overrides it only if it must. A refusal is an error of kind `PermissionDenied` naming the
/// path and why.
pub trait Fs {
    /// Makes one directory, private, failing when something is already there.
    fn create_private_dir(path: &Path) -> io::Result<()>;

    /// Opens a file of ours as `how` says.
    fn open_private(path: &Path, how: Create) -> io::Result<File>;

    /// Opens `path` (a file or a directory) for reading: a link there is refused, not followed, and nothing is
    /// waited on (a FIFO or a device put in a file's place).
    fn open_read(path: &Path) -> io::Result<File>;

    /// Makes what was just created, renamed or removed in the directory `dir` durable, where the OS has a way: Unix
    /// syncs the directory; Windows does nothing (NTFS journals its metadata and gives no handle to flush).
    fn sync_dir(dir: &Path) -> io::Result<()>;

    /// What others may do with the file or directory behind an open handle.
    fn others_of(file: &File) -> io::Result<Others>;

    /// Which file is behind an open handle (Unix: device and inode; Windows: volume serial number and the 128-bit
    /// file id, ReFS needing all of it; the 64-bit file index where the file system has no such id), read from the
    /// handle itself: the file being read, not whatever its path names a moment later.
    fn identity(file: &File) -> io::Result<FileId>;

    /// A private directory at the absolute `path`, made (with any missing parents, each private) when it does not
    /// exist, then checked with every directory on the way to it. The directory itself (a link to one is not) must be
    /// this user's and open to nobody else; each directory on the way this user's or the system's with nobody else
    /// able to replace what is in it. Only the path as written is judged, never where a link on the way leads, so a
    /// link on the way is refused unless the system made it: Unix judges each entry itself, owned by this user or
    /// root, not writable by group or others unless sticky (`/tmp`), and a link only when root's (macOS's `/var`,
    /// `/tmp`; Linux gives every link mode `0777`, so none passes there). Windows refuses any reparse point on the way
    /// (junction, symlink, mount point) and judges each directory: owned by this user, SYSTEM, Administrators or
    /// TrustedInstaller, nobody but this user and the system allowed to delete or rename what is in it or to take it
    /// over. An existing directory with the wrong owner or access is refused, not repaired.
    fn ensure_private_dir(path: &Path) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{} is not an absolute path", path.display()),
            ));
        }
        create_all::<Self>(path)?;
        let dir = Self::open_read(path).map_err(|error| refused(path, error))?;
        if !dir.metadata()?.is_dir() {
            return Err(refused(path, "not a directory"));
        }
        Self::check_private(&dir).map_err(|error| refused(path, error))?;
        for ancestor in path.ancestors().skip(1) {
            imp::check_ancestor(ancestor)?;
        }
        Ok(())
    }

    /// That what is behind `file` is private: this user's and open to nobody else; a regular file also has no
    /// other name (a hard link somebody else made would see what is written to it). A directory or a regular file,
    /// judged from what is there; anything else is refused.
    fn check_private(file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !UserId::current().owns_file(file)? {
            return Err(denied("not this user's"));
        }
        if Self::others_of(file)? != Others::Nothing {
            return Err(denied("open to others"));
        }
        if !metadata.is_dir() && !(metadata.is_file() && imp::link_count(file)? == 1) {
            return Err(denied("not a directory or a regular file with one name"));
        }
        Ok(())
    }

    /// A directory of the person's that we read or write in (a home, an agent's configuration): a real directory,
    /// this user's, that nobody else can change. Returns its canonical path.
    fn check_user_directory(path: &Path) -> io::Result<PathBuf> {
        let dir = Self::open_read(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("missing user directory {}: {error}", path.display()),
            )
        })?;
        if !dir.metadata()?.is_dir()
            || !UserId::current().owns_file(&dir)?
            || Self::others_of(&dir)? == Others::Change
        {
            return Err(denied(format!("unsafe user directory {}", path.display())));
        }
        path.canonicalize()
    }

    /// The bytes of a file we trust, judged on the handle that was opened (no link followed), so the file judged is
    /// the file read: a regular file, this user's, that nobody else can change, at most `limit` bytes. `None` when
    /// it does not exist; refused otherwise.
    fn read_trusted(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let file = match Self::open_read(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) if fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) => {
                return Err(denied(format!("unsafe file {}: a link", path.display())))
            }
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file()
            || !UserId::current().owns_file(&file)?
            || Self::others_of(&file)? == Others::Change
        {
            return Err(denied(format!("unsafe file {}", path.display())));
        }
        read_bounded(file, path, limit).map(Some)
    }

    /// The bytes of a private file of ours, judged on the handle that was opened (no link followed) and read from
    /// that same handle: [`Fs::check_private`] (a regular file with one name, this user's, open to nobody else), at
    /// most `limit` bytes. `None` when it does not exist; refused otherwise.
    fn read_private(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let file = match Self::open_private_file(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        read_bounded(file, path, limit).map(Some)
    }

    /// A private file of ours opened for reading: [`Fs::check_private`] and a regular file, judged on the handle that
    /// is returned (no link followed), so what was judged is what is read. Nothing is changed: a file that fails is
    /// refused and left as it is.
    fn open_private_file(path: &Path) -> io::Result<File> {
        let file = Self::open_read(path).map_err(|error| match error.kind() {
            ErrorKind::NotFound => error,
            _ => refused_file(path, error),
        })?;
        Self::check_private(&file).map_err(|error| refused_file(path, error))?;
        if !file.metadata()?.is_file() {
            return Err(refused_file(path, "not a file"));
        }
        Ok(file)
    }

    /// [`Fs::open_private_file`] for a record [`Fs::create_private`] makes, after finishing a creation its writer did
    /// not: on Unix a writer stopped between linking the record in and removing its staging name leaves the record
    /// with that second name. It is removed only when the directory is private (judged on the handle every step goes
    /// through, no link followed), the record there is a regular file of this user's open to nobody else with exactly
    /// two names, and the other is exactly the staging name the writer gives (`.<name>.` and its random part), the
    /// same file; otherwise nothing is touched and the record is judged as it is. Windows' writer never leaves a
    /// second name, so there is nothing to finish there.
    ///
    /// Readers may race each other and the writer: a staging name another one removed first is not an error, and a
    /// record refused while a live writer had linked it in but not yet removed its staging name is finished and
    /// judged once more.
    fn open_created_file(path: &Path) -> io::Result<File> {
        imp::finish_creation(path)?;
        match Self::open_private_file(path) {
            Err(error) if error.kind() == ErrorKind::PermissionDenied => {
                imp::finish_creation(path)?;
                Self::open_private_file(path)
            }
            opened => opened,
        }
    }

    /// Writes `bytes` as `path`, private, whole or not at all, in a directory that must be private: judged on the
    /// handle that is then written through (Unix: no link followed, every step relative to it; Windows: held open so
    /// it cannot be renamed or replaced meanwhile). The temporary file it is written to is private from its creation,
    /// before any byte is in it.
    fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        imp::write_whole(path, bytes, Publish::Private)
    }

    /// As [`Fs::write_private`], for a record made once and never replaced: `AlreadyExists` when something is at
    /// `path`, left as it is. The staged file gets the record's name with nothing replaced (Unix: a hard link, its
    /// staging name removed before the directory is synced; Windows: a rename that replaces nothing). Read it back
    /// with [`Fs::open_created_file`].
    fn create_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        imp::write_whole(path, bytes, Publish::New)
    }

    /// [`Fs::write_private`] of `value` as JSON.
    fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
        Self::write_private(path, &bytes)
    }

    /// Writes `bytes` as `path` whole or not at all, in a directory the caller has vouched for: an existing file
    /// keeps its access (Unix), a new one is private (Windows: the replacement always is, private from its creation).
    fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
        imp::write_whole(path, bytes, Publish::KeepAccess)
    }

    /// As [`Fs::replace`], but the file written is always private, whatever the one it replaces allowed: for a file
    /// left in a directory of the user's that need not be private (a failure report).
    fn replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
        imp::write_whole(path, bytes, Publish::Vouched)
    }
}

/// Makes `path` and any missing parents, each private; what is already there is left as it is.
fn create_all<F: Fs + ?Sized>(path: &Path) -> io::Result<()> {
    let created = match F::create_private_dir(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                create_all::<F>(parent)?;
            }
            F::create_private_dir(path)
        }
        other => other,
    };
    match created {
        Err(error) if error.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        other => other,
    }
}

/// How `imp::write_whole` puts `bytes` in place as a record: whole or not at all, staged in a new file beside it
/// under the staging name ([`STAGING_RANDOM`]), created private before any byte is written, synced, then given the
/// record's name, the directory synced; the staging file is removed on any failure.
#[derive(Clone, Copy, PartialEq)]
enum Publish {
    /// In a private directory, judged on the handle written through ([`judge_dir`]); renamed over the record; the
    /// replacement is private.
    Private,
    /// As `Private`, given the record's name only when nothing has it (`AlreadyExists` otherwise, what is there
    /// untouched).
    New,
    /// In a directory the caller vouched for, not judged; renamed over the record; the replacement is private.
    Vouched,
    /// As `Vouched`, taking the mode the record has now (Unix; a Windows replacement is private).
    KeepAccess,
}

impl Publish {
    /// Whether the directory must be private, judged on the handle written through.
    fn judged(self) -> bool {
        matches!(self, Publish::Private | Publish::New)
    }
}

/// How many random characters `tempfile` adds to a staging name (its default, written out because
/// [`is_staging_name`] relies on it): `.<name>.` and that many ASCII letters and digits.
const STAGING_RANDOM: usize = 6;

/// The staging name of a record to write at `path`: `.<name>.`, then [`STAGING_RANDOM`] characters `tempfile` adds.
fn staging_prefix(path: &Path) -> String {
    format!(
        ".{}.",
        path.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// Whether `candidate` is exactly a staging name the writer gives the record `record`: `.<record>.` and
/// [`STAGING_RANDOM`] ASCII letters or digits, nothing else.
#[cfg_attr(windows, allow(dead_code))]
fn is_staging_name(record: &std::ffi::OsStr, candidate: &std::ffi::OsStr) -> bool {
    let (record, candidate) = (record.as_encoded_bytes(), candidate.as_encoded_bytes());
    let Some(rest) = candidate
        .strip_prefix(b".")
        .and_then(|rest| rest.strip_prefix(record))
        .and_then(|rest| rest.strip_prefix(b"."))
    else {
        return false;
    };
    rest.len() == STAGING_RANDOM && rest.iter().all(u8::is_ascii_alphanumeric)
}

/// The directory `path` is in, refused when it names none.
fn parent_of(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "a file with no parent"))
}

/// That the directory `dir` (at `path`) is a private one, judged on the handle: a directory, [`Fs::check_private`].
fn judge_dir<F: Fs + ?Sized>(path: &Path, dir: &File) -> io::Result<()> {
    if !dir.metadata()?.is_dir() {
        return Err(refused(path, "not a directory"));
    }
    F::check_private(dir).map_err(|error| refused(path, error))
}

/// At most `limit` bytes from `file`, refused when there are more.
fn read_bounded(file: File, path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("{} is too large: over {limit} bytes", path.display()),
        ));
    }
    Ok(bytes)
}

/// A refusal: `PermissionDenied`, saying why.
fn denied(why: impl std::fmt::Display) -> io::Error {
    io::Error::new(ErrorKind::PermissionDenied, why.to_string())
}

/// `path` refused as a private directory, saying why.
fn refused(path: &Path, why: impl std::fmt::Display) -> io::Error {
    denied(format!(
        "unsafe private directory {}: {why}",
        path.display()
    ))
}

/// `path` refused as a private file, saying why.
fn refused_file(path: &Path, why: impl std::fmt::Display) -> io::Error {
    denied(format!("unsafe private file {}: {why}", path.display()))
}

/// What tests do to files and the interface above never does: make a link, mark a program, widen and narrow the
/// access to a path. Only in this crate's own tests and in a build with the `test-support` feature (a dependent's
/// tests, through a dev-dependency on this crate with it), never in a program. Implemented by [`Files`]; callers
/// import both: `use sidevoice_platform::fs::{Files, TestFiles};` then `Files::symlink(..)`.
#[cfg(any(test, feature = "test-support"))]
pub trait TestFiles {
    /// A symbolic link at `link` to `target` (a directory or a file), for tests of what must never be followed.
    fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>);

    /// Makes `path` something a test may run (a script standing in for an agent): executable by its owner on Unix.
    /// Nothing on Windows, where the extension decides.
    fn make_executable(path: &Path);

    /// Lets everybody else read (and enter) `path`, as a directory made with a loose mask is: group and others' read
    /// and search bits on Unix, a read grant to Everyone on Windows.
    fn let_others_read(path: &Path);

    /// Lets everybody else change `path`: group and others' bits on Unix, a full-control grant to Everyone on Windows.
    fn let_others_change(path: &Path);

    /// Narrows `path` to its owner: group and others' bits cleared on Unix (also what the umask gave a new file); on
    /// Windows its DACL protected, what it inherited kept as its own (in a test's scratch directory the owner's
    /// access, SYSTEM's and the administrators', and nobody else's), and the grants above to Everyone removed.
    fn shut_others_out(path: &Path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A scratch directory under the temporary directory's canonical path (macOS: `/private/var/...`, not the `/var`
    /// link), so what the code canonicalizes equals what a test built. Not private: a test that needs a private
    /// directory makes one inside.
    fn scratch() -> tempfile::TempDir {
        tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap()
    }

    /// Whether what `path` names is private, judged on a handle opened without following a link.
    fn private(path: &Path) -> bool {
        Files::open_read(path)
            .and_then(|file| Files::check_private(&file))
            .is_ok()
    }

    fn others(path: &Path) -> Others {
        Files::others_of(&Files::open_read(path).unwrap()).unwrap()
    }

    fn refused<T: std::fmt::Debug>(result: io::Result<T>) -> bool {
        result.unwrap_err().kind() == ErrorKind::PermissionDenied
    }

    #[test]
    fn a_private_directory_is_made_with_its_missing_parents_owned_and_open_to_nobody_else() {
        let root = scratch();
        let dir = root.path().join("a").join("b");
        Files::ensure_private_dir(&dir).unwrap();
        Files::ensure_private_dir(&dir).unwrap();
        assert!(private(&dir));
        assert!(private(dir.parent().unwrap()), "each parent it made");
        assert!(UserId::current().owns_path(&dir).unwrap());
        assert_eq!(others(&dir), Others::Nothing);
        assert_eq!(
            Files::ensure_private_dir(Path::new("relative/dir"))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_directory_somebody_else_can_change_is_refused_not_repaired() {
        let root = scratch();
        let dir = root.path().join("data");
        Files::ensure_private_dir(&dir).unwrap();
        Files::let_others_change(&dir);
        assert_eq!(others(&dir), Others::Change);
        assert!(!private(&dir));
        assert!(refused(Files::ensure_private_dir(&dir)));
        assert_eq!(others(&dir), Others::Change, "not repaired");
        assert!(Files::check_user_directory(&dir).is_err());
        assert!(
            refused(Files::ensure_private_dir(&dir.join("inside"))),
            "nor trusted on the way to something private"
        );
    }

    #[test]
    fn a_directory_others_can_read_is_not_private() {
        let root = scratch();
        let dir = root.path().join("data");
        Files::ensure_private_dir(&dir).unwrap();
        Files::let_others_read(&dir);
        assert_eq!(others(&dir), Others::Read);
        assert!(refused(Files::ensure_private_dir(&dir)));
        Files::shut_others_out(&dir);
        Files::ensure_private_dir(&dir).unwrap();
    }

    #[test]
    fn a_file_and_a_link_are_not_private_directories() {
        let root = scratch();
        let file = root.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(Files::ensure_private_dir(&file).is_err());
        let target = root.path().join("target");
        Files::ensure_private_dir(&target).unwrap();
        let link = root.path().join("link");
        Files::symlink(&target, &link);
        assert!(
            refused(Files::ensure_private_dir(&link)),
            "a link is not the directory it leads to"
        );
    }

    #[test]
    fn a_private_directory_under_one_others_can_change_is_refused() {
        let root = scratch();
        let open = root.path().join("open");
        let dir = open.join("data");
        Files::ensure_private_dir(&dir).unwrap();
        Files::let_others_change(&open);
        assert!(
            refused(Files::ensure_private_dir(&dir)),
            "others could replace it"
        );
        assert!(
            refused(Files::ensure_private_dir(&dir.join("..").join("data"))),
            "however it is spelled"
        );
    }

    #[test]
    fn a_private_directory_reached_through_a_link_on_the_way_is_refused() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::ensure_private_dir(&dir).unwrap();
        let link = root.path().join("link");
        Files::symlink(&dir, &link);
        Files::create_private_dir(&dir.join("inside")).unwrap();
        assert!(
            refused(Files::ensure_private_dir(&link.join("inside"))),
            "a link of this user's on the way is refused, not followed"
        );
    }

    #[test]
    fn a_private_write_replaces_the_record_whole_and_leaves_no_temporary_file() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("state.json");
        Files::write_private(&file, b"first").unwrap();
        Files::write_private(&file, b"second").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"second");
        assert!(private(&file));
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
        let open = root.path().join("open");
        Files::create_private_dir(&open).unwrap();
        Files::let_others_change(&open);
        assert!(
            refused(Files::write_private(&open.join("other.json"), b"x")),
            "never written where others can enter"
        );
    }

    #[test]
    fn a_write_over_a_file_others_can_read_leaves_a_private_one() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("state.json");
        fs::write(&file, b"old").unwrap();
        Files::let_others_read(&file);
        assert!(!private(&file));
        Files::write_private(&file, b"new").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"new");
        assert!(private(&file), "the replacement is private");
    }

    #[test]
    fn a_write_that_cannot_replace_the_record_leaves_no_temporary_file() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("state.json");
        fs::create_dir(&file).unwrap();
        fs::write(file.join("inside"), b"x").unwrap();
        assert!(Files::write_private(&file, b"new").is_err());
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
    }

    #[test]
    fn a_record_made_once_is_never_replaced_and_leaves_no_staging_name() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("identity.json");
        Files::create_private(&file, b"first").unwrap();
        let again = Files::create_private(&file, b"second").unwrap_err();
        assert_eq!(again.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&file).unwrap(), b"first");
        assert!(private(&file), "one name: the staging one is gone");
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["identity.json"]);
    }

    #[test]
    fn a_staging_name_is_exactly_the_one_the_writer_gives() {
        let record = std::ffi::OsStr::new("identity.json");
        let is = |name: &str| is_staging_name(record, std::ffi::OsStr::new(name));
        assert!(is(".identity.json.a1B2c3"));
        for name in [
            "identity.json",
            ".identity.json.",
            ".identity.json.a1B2c",
            ".identity.json.a1B2c3d",
            ".identity.json.a1-2c3",
            ".identity.json.a1B2c3.bak",
            ".other.json.a1B2c3",
            "identity.json.a1B2c3",
        ] {
            assert!(!is(name), "{name}");
        }
    }

    // Unix only: only the Unix writer gives a record made once its name by a link, which a stop can leave with a
    // second one; Windows renames without replacing, so the state cannot arise there and `open_created_file` has
    // nothing to finish.
    #[cfg(unix)]
    #[test]
    fn a_creation_its_writer_did_not_finish_is_finished_only_where_asked_and_only_when_it_is_ours()
    {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("identity.json");
        Files::create_private(&file, b"made").unwrap();
        // The writer stopped after linking the record in, before removing its staging name.
        let staged = dir.join(".identity.json.a1B2c3");
        fs::hard_link(&file, &staged).unwrap();
        assert!(
            refused(Files::read_private(&file, 64)),
            "an ordinary read repairs nothing"
        );
        assert!(staged.exists(), "left as it is");
        let mut read = Vec::new();
        Files::open_created_file(&file)
            .unwrap()
            .read_to_end(&mut read)
            .unwrap();
        assert_eq!(read, b"made");
        assert!(!staged.exists(), "the staging name is removed");
        assert!(private(&file));
        // A second name that is not exactly its staging name, or not beside it, is somebody's: left, and refused.
        for other in [
            dir.join("other.json"),
            dir.join(".identity.json.backup-1"),
            root.path().join(".identity.json.d4E5f6"),
        ] {
            fs::hard_link(&file, &other).unwrap();
            assert!(refused(Files::open_created_file(&file)));
            assert!(other.exists(), "left as it is");
            fs::remove_file(&other).unwrap();
        }
        // A staging name of another record's is not this one's.
        Files::create_private(&dir.join("other.json"), b"other").unwrap();
        fs::hard_link(&file, dir.join(".other.json.g7H8i9")).unwrap();
        assert!(refused(Files::open_created_file(&file)));
        fs::remove_file(dir.join(".other.json.g7H8i9")).unwrap();
        // A record others can read is refused before any name is removed.
        fs::hard_link(&file, &staged).unwrap();
        Files::let_others_read(&file);
        assert!(refused(Files::open_created_file(&file)));
        assert!(
            staged.exists(),
            "nothing removed for a record that is not private"
        );
        fs::remove_file(&staged).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        // Nor where others can put names.
        let open = root.path().join("open");
        Files::create_private_dir(&open).unwrap();
        let shared = open.join("identity.json");
        Files::create_private(&shared, b"made").unwrap();
        fs::hard_link(&shared, open.join(".identity.json.j1K2l3")).unwrap();
        Files::let_others_change(&open);
        assert!(refused(Files::open_created_file(&shared)));
        assert!(open.join(".identity.json.j1K2l3").exists());
        // Nor through a link to the directory.
        let link = root.path().join("link");
        Files::shut_others_out(&open);
        Files::symlink(&open, &link);
        assert!(refused(Files::open_created_file(
            &link.join("identity.json")
        )));
        assert!(open.join(".identity.json.j1K2l3").exists());
    }

    // Unix only, as the test above: only there can a creation be left unfinished.
    #[cfg(unix)]
    #[test]
    fn readers_racing_each_other_and_the_writer_finish_a_creation_without_errors() {
        let root = scratch();
        for round in 0..100 {
            let dir = root.path().join(format!("private-{round}"));
            Files::create_private_dir(&dir).unwrap();
            // Several readers find the same unfinished creation: each one gets the record, none is told it is missing.
            let file = dir.join("identity.json");
            Files::create_private(&file, b"made").unwrap();
            fs::hard_link(&file, dir.join(".identity.json.a1B2c3")).unwrap();
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(|| Files::open_created_file(&file).unwrap());
                }
            });
            assert!(private(&file));
            // Readers race the writer: the writer succeeds even when a reader removed its staging name first, and a
            // reader sees either no record yet or the record, never a refusal.
            let next = dir.join("next.json");
            std::thread::scope(|scope| {
                scope.spawn(|| Files::create_private(&next, b"next").unwrap());
                for _ in 0..3 {
                    scope.spawn(|| loop {
                        match Files::open_created_file(&next) {
                            Ok(_) => break,
                            Err(error) if error.kind() == ErrorKind::NotFound => {
                                std::thread::yield_now()
                            }
                            Err(error) => panic!("{error}"),
                        }
                    });
                }
            });
            assert!(private(&next));
            assert_eq!(
                fs::read_dir(&dir).unwrap().count(),
                2,
                "no staging name left"
            );
        }
    }

    #[test]
    fn a_private_write_through_a_link_to_a_private_directory_is_refused() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let link = root.path().join("link");
        Files::symlink(&dir, &link);
        assert!(Files::write_private(&link.join("state.json"), b"secret").is_err());
        assert!(Files::create_private(&link.join("identity.json"), b"secret").is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "nothing written");
    }

    #[test]
    fn a_private_replacement_is_private_whatever_the_file_it_replaces_allowed() {
        let root = scratch();
        let file = root.path().join("core-failure.json");
        fs::write(&file, b"{}").unwrap();
        Files::let_others_change(&file);
        Files::replace_private(&file, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"{\"a\":1}");
        assert!(private(&file));
        assert!(Files::read_trusted(&file, 64).unwrap().is_some());
    }

    #[test]
    fn a_value_is_written_as_json() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("value.json");
        Files::write_json(&file, &serde_json::json!({"a": [1]})).unwrap();
        assert_eq!(fs::read(&file).unwrap(), br#"{"a":[1]}"#);
        assert!(private(&file));
    }

    #[test]
    fn a_replaced_file_is_written_whole_and_a_new_one_is_private() {
        let root = scratch();
        let file = root.path().join("config.json");
        Files::replace(&file, b"{}").unwrap();
        assert!(private(&file));
        Files::replace(&file, b"{\"a\":1}").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn a_trusted_file_is_read_bounded_and_refused_once_somebody_else_can_change_it() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("record.json");
        assert!(Files::read_trusted(&file, 1024).unwrap().is_none());
        Files::write_private(&file, br#"{"a":1}"#).unwrap();
        assert_eq!(
            Files::read_trusted(&file, 1024).unwrap().unwrap(),
            br#"{"a":1}"#
        );
        assert!(Files::read_trusted(&file, 3).is_err(), "oversized");
        assert!(
            refused(Files::read_trusted(&dir, 1024)),
            "not a regular file"
        );
        Files::let_others_change(&file);
        assert!(refused(Files::read_trusted(&file, 1024)));
        assert!(!private(&file));
    }

    #[test]
    fn a_private_file_is_read_from_the_handle_that_was_judged() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let file = dir.join("record.json");
        assert!(Files::read_private(&file, 1024).unwrap().is_none());
        Files::write_private(&file, b"[1]").unwrap();
        assert_eq!(Files::read_private(&file, 1024).unwrap().unwrap(), b"[1]");
        assert!(Files::read_private(&file, 2).is_err(), "oversized");
        assert!(refused(Files::read_private(&dir, 1024)), "not a file");
        let link = dir.join("link.json");
        Files::symlink(&file, &link);
        assert!(refused(Files::read_private(&link, 1024)), "a link");
        Files::let_others_read(&file);
        assert!(
            refused(Files::read_private(&file, 1024)),
            "others can read it"
        );
        assert!(
            Files::read_trusted(&file, 1024).unwrap().is_some(),
            "still a file nobody else can change"
        );
    }

    #[test]
    fn the_private_openers_make_private_files_and_keep_what_is_in_them() {
        let root = scratch();
        let log = root.path().join("log");
        writeln!(Files::open_private(&log, Create::Append).unwrap(), "one").unwrap();
        writeln!(Files::open_private(&log, Create::Append).unwrap(), "two").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        assert!(private(&log));
        let held = Files::open_private(&log, Create::ReadWrite).unwrap();
        Files::check_private(&held).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n", "kept");
        let new = root.path().join("new");
        let _held = Files::open_private(&new, Create::New).unwrap();
        assert!(private(&new), "private before anything is written to it");
        assert!(Files::open_private(&new, Create::New).is_err(), "exclusive");
        let program = root.path().join("program");
        Files::open_private(&program, Create::NewProgram).unwrap();
        assert!(private(&program));
    }

    #[test]
    fn a_link_in_the_place_of_a_private_file_is_not_followed() {
        let root = scratch();
        let target = root.path().join("somebody-else");
        fs::write(&target, b"untouched").unwrap();
        let link = root.path().join("log");
        Files::symlink(&target, &link);
        assert!(Files::open_private(&link, Create::Append).is_err());
        assert!(Files::open_private(&link, Create::ReadWrite).is_err());
        assert!(Files::open_read(&link).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
    }

    #[test]
    fn a_private_file_is_a_regular_file_or_a_directory_of_this_user_s_with_one_name() {
        let root = scratch();
        let file = root.path().join("lock");
        let handle = Files::open_private(&file, Create::ReadWrite).unwrap();
        Files::check_private(&handle).unwrap();
        fs::hard_link(&file, root.path().join("other")).unwrap();
        assert!(
            refused(Files::check_private(&handle)),
            "a second name, seen on the handle"
        );
        let dir = root.path().join("dir");
        Files::create_private_dir(&dir).unwrap();
        Files::check_private(&Files::open_read(&dir).unwrap()).unwrap();
        assert!(
            refused(Files::open_private_file(&dir)),
            "a directory is not a file"
        );
    }

    #[test]
    fn a_file_is_told_apart_from_the_one_that_takes_its_path() {
        let root = scratch();
        let (path, next) = (root.path().join("log"), root.path().join("next"));
        fs::write(&path, b"first").unwrap();
        let first = Files::identity(&File::open(&path).unwrap()).unwrap();
        assert!(first == Files::identity(&File::open(&path).unwrap()).unwrap());
        // Rotated: another file takes the path while the first still exists.
        fs::write(&next, b"second").unwrap();
        fs::rename(&next, &path).unwrap();
        assert!(first != Files::identity(&File::open(&path).unwrap()).unwrap());
    }

    #[test]
    fn a_user_directory_is_the_users_and_named_canonically() {
        let root = scratch();
        assert_eq!(
            Files::check_user_directory(root.path()).unwrap(),
            root.path().canonicalize().unwrap()
        );
        let file = root.path().join("config.toml");
        fs::write(&file, "a = 1\n").unwrap();
        assert!(
            Files::check_user_directory(&file).is_err(),
            "not a directory"
        );
        assert!(Files::check_user_directory(&root.path().join("missing")).is_err());
    }

    #[test]
    fn a_new_file_in_a_directory_others_can_read_is_still_private() {
        let root = scratch();
        let shared = root.path().join("shared");
        Files::create_private_dir(&shared).unwrap();
        Files::let_others_read(&shared);
        let file = shared.join("config.json");
        Files::replace(&file, b"{}").unwrap();
        assert!(private(&file), "not what its directory lets others do");
    }

    #[test]
    fn a_directory_can_be_synced() {
        let root = scratch();
        Files::sync_dir(root.path()).unwrap();
    }

    // Windows only: an entry only what is created inside inherits, and a DACL that takes its parent's, have no Unix
    // counterpart (a directory's mode is not passed down).
    #[cfg(windows)]
    #[test]
    fn a_directory_that_passes_access_down_to_others_is_not_private() {
        let root = scratch();
        let dir = root.path().join("files-readable");
        Files::create_private_dir(&dir).unwrap();
        imp::icacls(&dir, &["/grant", "*S-1-1-0:(OI)(IO)(R)"]);
        assert_eq!(others(&dir), Others::Read);
        assert!(
            refused(Files::ensure_private_dir(&dir)),
            "what is created in it would be readable by everybody"
        );
        assert!(Files::write_private(&dir.join("x.json"), b"{}").is_err());
        let file = dir.join("config.json");
        Files::replace(&file, b"{}").unwrap();
        assert!(
            private(&file),
            "created with its own DACL, nothing inherited"
        );
    }

    // Windows only, as above.
    #[cfg(windows)]
    #[test]
    fn a_directory_whose_dacl_is_not_protected_is_not_private() {
        let root = scratch();
        let dir = root.path().join("inheriting");
        Files::create_private_dir(&dir).unwrap();
        imp::icacls(&dir, &["/inheritance:e"]);
        assert_ne!(others(&dir), Others::Nothing);
        assert!(
            refused(Files::ensure_private_dir(&dir)),
            "it takes whatever its parent passes down"
        );
        assert!(
            Files::check_user_directory(&dir).is_ok(),
            "still a directory nobody else can change"
        );
    }

    // Windows only: a profile folder owned by Administrators (or SYSTEM) granting its user full control has no Unix
    // counterpart, where a directory has one owner and mode bits; `/setowner` to Administrators needs the elevated
    // token the CI runner has.
    #[cfg(windows)]
    #[test]
    fn a_directory_on_the_way_owned_by_administrators_granting_this_user_is_trusted() {
        let root = scratch();
        let profile = root.path().join("profile");
        fs::create_dir(&profile).unwrap();
        let user = UserId::name().unwrap();
        imp::icacls(&profile, &["/grant", &format!("{user}:(OI)(CI)(F)")]);
        imp::icacls(&profile, &["/setowner", "*S-1-5-32-544"]);
        let dir = profile.join("AppData").join("Local").join("data");
        Files::ensure_private_dir(&dir).unwrap();
    }

    // Windows only: a junction (which needs no privilege to make) is Windows' own kind of link; the generic tests
    // above cover symbolic links on every OS.
    #[cfg(windows)]
    #[test]
    fn a_junction_on_the_way_to_a_private_directory_is_refused() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::ensure_private_dir(&dir).unwrap();
        let link = root.path().join("link");
        imp::link_dir(&dir, &link);
        assert!(
            refused(Files::ensure_private_dir(&link.join("inside"))),
            "a link on the way is refused, not followed"
        );
    }
}
