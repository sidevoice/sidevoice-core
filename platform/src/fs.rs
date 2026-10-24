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
//!
//! [`Fs::trusted_program`] validates Sidevoice's own programs, including system installs, through the same held
//! ancestry. Its returned path is not descriptor-bound execution; see the method's documented launch limit.

#[cfg(unix)]
#[path = "fs/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "fs/windows.rs"]
mod imp;

pub use imp::{Held as HeldDirectory, Os as Files};

use crate::user::{User, UserId};
use serde::Serialize;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

/// Required access to an existing directory or record. Checks never repair permissions.
#[derive(Clone, Copy)]
pub enum Access {
    /// This user's, open to nobody else (apart from the system).
    Private,
    /// This user's, readable by others but never writable by them. Windows directories may also belong to the
    /// system, as ordinary profile folders do. Files must still belong to this user and have exactly one name.
    NotWritableByOthers,
}

/// Existing-only access through a checked directory and its held ancestry ([`HeldDirectory`]).
///
/// No operation creates a directory or repairs existing permissions. Links/reparse points, unsafe ownership,
/// hard-linked records and replaced ancestors are refused with `PermissionDenied`. On Unix operations are relative
/// to the held directory; Windows holds every ancestor without delete sharing. Same-user/admin modification is
/// outside this boundary. Reads return `None` only for an absent child; an absent directory is `NotFound`.
///
/// Import both: `use sidevoice_platform::fs::{Directory, HeldDirectory, Access};`.
pub trait Directory: Sized {
    /// Opens an absolute existing directory, checking every ancestor. Missing directories remain missing. Windows
    /// redirected/junction roots are refused: choose an ordinary, unredirected data directory instead. Unix retains
    /// the system-owned ancestor-link allowance of [`Fs::ensure_private_dir`]. Parent (`..`) components are invalid.
    fn open(path: &Path, access: Access) -> io::Result<Self>;

    /// Reads a regular record from the checked handle, bounded by `limit` (`InvalidData` when exceeded).
    /// Concurrent replacement can be refused if the opened record loses its name before the checks finish.
    /// `name` must be one ordinary child component, with no separators, NUL, `.` or `..`; Windows also refuses device
    /// names, alternate data streams and trailing dot/space aliases. Invalid names return `InvalidInput`.
    fn read(&self, name: &OsStr, access: Access, limit: u64) -> io::Result<Option<Vec<u8>>>;

    /// Atomically publishes a private record through this directory, using the same staging/sync/publication as
    /// [`Fs::write_private`]. An existing record must satisfy `NotWritableByOthers`; unsafe state is never repaired.
    /// Readable-but-not-writable-by-others directories are accepted. Windows has no directory flush guarantee.
    fn write(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()>;

    /// Removes one regular record satisfying `NotWritableByOthers`, never a directory or link. An absent child is
    /// success. The parent is synced where supported. Name validation is the same as for [`Directory::read`].
    fn remove(&self, name: &OsStr) -> io::Result<()>;
}

/// A root-to-leaf sequence, kept alive for the entire capability. Per-OS code controls how it is opened/reopened.
struct HeldState {
    chain: Vec<(PathBuf, File)>,
    // None applies the ancestor policy to the leaf too (a system or user program directory).
    access: Option<Access>,
}

impl HeldState {
    fn leaf(&self) -> (&Path, &File) {
        let (path, file) = self.chain.last().expect("an absolute directory has a root");
        (path, file)
    }

    fn verify(&self) -> io::Result<()> {
        for (index, (path, held)) in self.chain.iter().enumerate() {
            let leaf = index + 1 == self.chain.len();
            let current =
                imp::reopen_directory(path, leaf).map_err(|error| refused(path, error))?;
            if Files::identity(held)? != Files::identity(&current)? {
                return Err(refused(
                    path,
                    "directory replaced; reopen the verified data directory",
                ));
            }
            if let Some(access) = self.access.filter(|_| leaf) {
                judge_access(held, access, true).map_err(|error| refused(path, error))?;
            } else {
                imp::check_ancestor_handle(path, held)?;
            }
        }
        Ok(())
    }
}

fn directory_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "expected an absolute directory without '..'",
        ));
    }
    let mut paths: Vec<_> = path.ancestors().map(Path::to_path_buf).collect();
    paths.reverse();
    Ok(paths)
}

fn child_name(name: &OsStr) -> io::Result<&Path> {
    let path = Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
        || name
            .as_encoded_bytes()
            .iter()
            .any(|b| matches!(b, b'/' | b'\\' | 0))
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "expected one ordinary child name",
        ));
    }
    Ok(path)
}

fn program_name(path: &Path) -> io::Result<&OsStr> {
    directory_paths(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "expected a program file name"))?;
    child_name(name)?;
    Ok(name)
}

fn judge_access(file: &File, access: Access, directory: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    if (directory && !metadata.is_dir())
        || (!directory && (!metadata.is_file() || imp::link_count(file)? != 1))
    {
        return Err(denied("not a directory or a regular record with one name"));
    }
    match access {
        Access::Private => Files::check_private(file),
        Access::NotWritableByOthers => {
            let owner = if directory {
                Files::owner_of_user_directory_is_trusted(file)?
            } else {
                UserId::current().owns_file(file)?
            };
            if !owner || Files::others_of(file)? == Others::Change {
                return Err(denied("unsafe owner or writable by others"));
            }
            Ok(())
        }
    }
}

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

    /// Validates a recorded Sidevoice program and returns the absolute path to launch. This is not for agent
    /// CLIs, PATH discovery or publisher authentication. Nothing is created, repaired or executed.
    ///
    /// The opened regular file must belong to this user or the system (Unix root; Windows SYSTEM, Administrators
    /// or TrustedInstaller), with no write/takeover rights granted to other users. Its ancestry is held and checked
    /// with the same mechanism as [`Directory`], including system-owned program directories and Unix sticky parents.
    /// Unix requires an execute bit and rejects a sticky file. Windows accepts native `.exe` names, case-insensitively;
    /// it does not consult PATHEXT or invoke a shell. The OS loader still decides whether the payload can execute.
    /// Unlike private records, programs may have hard links: they confer no write rights or secret-file access.
    ///
    /// Supply an absolute path without `..`. Unix resolves links first and checks the resolved ancestry and file;
    /// launch the returned path, never the original link. Windows refuses every reparse point, including a final
    /// link or junctioned root: reinstall in an ordinary, unredirected directory. Missing paths return `NotFound`,
    /// unsafe files/ancestry `PermissionDenied`, invalid names `InvalidInput`; other IO failures propagate.
    ///
    /// **Path-launch limit:** validation releases its handles before returning. Neither Unix path execution nor
    /// Windows CreateProcess is bound to the checked handle. Revalidate immediately before launching, but this
    /// cannot prevent a same-user/admin replacement, in-place edit, or changed interpreter/library. Those actors
    /// are outside the boundary; this is neither a content hash nor a signature check.
    fn trusted_program(path: &Path) -> io::Result<PathBuf>;

    /// Makes what was just created, renamed or removed in the directory `dir` durable, where the OS has a way: Unix
    /// syncs the directory; Windows does nothing (NTFS journals its metadata and gives no handle to flush).
    fn sync_dir(dir: &Path) -> io::Result<()>;

    /// What others may do with the file or directory behind an open handle.
    fn others_of(file: &File) -> io::Result<Others>;

    /// Whether a directory of the person's (a home, an agent's configuration) behind an open handle has an owner we
    /// accept: this user (Unix); this user or the system (Windows: a profile folder `C:\Users\<name>` is owned by
    /// SYSTEM or Administrators and grants its user full control, the default layout).
    fn owner_of_user_directory_is_trusted(dir: &File) -> io::Result<bool>;

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
            || !Self::owner_of_user_directory_is_trusted(&dir)?
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

    fn held(path: &Path, access: Access) -> HeldDirectory {
        HeldDirectory::open(path, access).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn assert_open_refused(path: &Path, access: Access) {
        assert_eq!(
            HeldDirectory::open(path, access).err().unwrap().kind(),
            ErrorKind::PermissionDenied
        );
    }

    fn program(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        Files::open_private(&path, Create::NewProgram)
            .unwrap()
            .write_all(b"test payload; the OS loader checks its format")
            .unwrap();
        path
    }

    #[test]
    fn trusted_program_accepts_user_installs_and_readable_program_directories() {
        let root = scratch();
        let dir = root.path().join("Sidevoice café tools");
        Files::create_private_dir(&dir).unwrap();
        let path = program(&dir, "sidevoice-connector.EXE");
        Files::let_others_read(&dir);
        Files::let_others_read(&path);
        assert_eq!(
            Files::trusted_program(&path).unwrap(),
            path.canonicalize().unwrap()
        );
        assert_eq!(others(&dir), Others::Read, "discovery never repairs access");
        assert_eq!(others(&path), Others::Read);
    }

    #[test]
    fn trusted_program_preserves_long_spaced_and_non_ascii_paths() {
        let root = scratch();
        let mut dir = root.path().to_path_buf();
        for _ in 0..10 {
            dir.push("Sidevoice café installation files");
            Files::create_private_dir(&dir).unwrap();
            // Extended-length spelling for subsequent raw Windows creation calls.
            dir = dir.canonicalize().unwrap();
        }
        let path = program(&dir, "sidevoice connector.exe");
        assert_eq!(
            Files::trusted_program(&path).unwrap(),
            path.canonicalize().unwrap()
        );
    }

    #[test]
    fn trusted_program_accepts_a_real_system_install() {
        // Only fixture selection differs; the same validator checks a real system-owned executable on every OS.
        let path = imp::system_program();
        assert_eq!(
            Files::trusted_program(&path).unwrap(),
            path.canonicalize().unwrap()
        );
    }

    #[test]
    fn trusted_program_refuses_writable_payload_and_ancestry_without_repair() {
        let root = scratch();
        let above = root.path().join("above");
        Files::create_private_dir(&above).unwrap();
        let dir = above.join("bin");
        Files::create_private_dir(&dir).unwrap();
        let path = program(&dir, "sidevoice.exe");
        for unsafe_path in [&path, &dir, &above] {
            Files::let_others_change(unsafe_path);
            assert!(
                refused(Files::trusted_program(&path)),
                "{}",
                unsafe_path.display()
            );
            assert_eq!(others(unsafe_path), Others::Change, "never repaired");
            Files::shut_others_out(unsafe_path);
        }
        Files::trusted_program(&path).unwrap();
    }

    #[test]
    fn trusted_program_discovery_preserves_absence_and_rejects_invalid_input() {
        let root = scratch();
        let absent = root.path().join("absent");
        for path in [
            root.path().join("missing.exe"),
            absent.join("sidevoice.exe"),
        ] {
            assert_eq!(
                Files::trusted_program(&path).unwrap_err().kind(),
                ErrorKind::NotFound
            );
        }
        assert!(!absent.exists(), "discovery never creates an install");
        // PathBuf::join normalizes `..` away under a Windows verbatim prefix. Build the invalid spelling
        // literally so the validator, rather than the fixture, sees and rejects the parent component.
        let mut parent = root.path().as_os_str().to_os_string();
        parent.push(std::path::MAIN_SEPARATOR_STR);
        parent.push("..");
        parent.push(std::path::MAIN_SEPARATOR_STR);
        parent.push("sidevoice.exe");
        for path in [PathBuf::from("sidevoice.exe"), PathBuf::from(parent)] {
            assert_eq!(
                Files::trusted_program(&path).unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
        }
        let directory = root.path().join("directory.exe");
        Files::create_private_dir(&directory).unwrap();
        assert!(refused(Files::trusted_program(&directory)));
    }

    #[test]
    fn trusted_program_hard_links_do_not_grant_write_rights() {
        let root = scratch();
        let path = program(root.path(), "sidevoice.exe");
        let alias = root.path().join("alias.exe");
        fs::hard_link(&path, &alias).unwrap();
        Files::trusted_program(&path).unwrap();
        Files::trusted_program(&alias).unwrap();
        Files::let_others_change(&alias);
        assert!(refused(Files::trusted_program(&path)));
        assert!(refused(Files::trusted_program(&alias)));
    }

    #[test]
    fn trusted_program_rechecks_replaced_files_and_directories() {
        let root = scratch();
        let dir = root.path().join("bin");
        Files::create_private_dir(&dir).unwrap();
        let path = program(&dir, "sidevoice.exe");
        let resolved = Files::trusted_program(&path).unwrap();
        fs::rename(&path, dir.join("old.exe")).unwrap();
        program(&dir, "sidevoice.exe");
        Files::let_others_change(&path);
        assert!(refused(Files::trusted_program(&resolved)));
        fs::rename(&dir, root.path().join("old-bin")).unwrap();
        Files::create_private_dir(&dir).unwrap();
        program(&dir, "sidevoice.exe");
        Files::let_others_change(&dir);
        assert!(refused(Files::trusted_program(&resolved)));
    }

    #[test]
    fn trusted_program_link_policy_never_returns_a_retargetable_link() {
        let root = scratch();
        let safe = program(root.path(), "safe.exe");
        let unsafe_program = program(root.path(), "unsafe.exe");
        Files::let_others_change(&unsafe_program);
        let link = root.path().join("sidevoice.exe");
        Files::symlink(&safe, &link);
        // The contract deliberately differs: Unix resolves once; Windows D1=A refuses all reparse points.
        #[cfg(unix)]
        let resolved = Files::trusted_program(&link).unwrap();
        #[cfg(windows)]
        assert!(refused(Files::trusted_program(&link)));
        fs::remove_file(&link).unwrap();
        Files::symlink(&unsafe_program, &link);
        assert!(refused(Files::trusted_program(&link)));
        #[cfg(unix)]
        {
            assert_eq!(resolved, safe.canonicalize().unwrap());
            Files::trusted_program(&resolved).unwrap();
        }
    }

    #[test]
    fn trusted_program_directory_link_policy_checks_the_target_or_refuses() {
        let root = scratch();
        let dir = root.path().join("bin");
        Files::create_private_dir(&dir).unwrap();
        let safe = program(&dir, "sidevoice.exe");
        let link = root.path().join("linked-bin");
        Files::symlink(&dir, &link);
        let path = link.join("sidevoice.exe");
        #[cfg(unix)]
        assert_eq!(
            Files::trusted_program(&path).unwrap(),
            safe.canonicalize().unwrap()
        );
        #[cfg(windows)]
        assert!(refused(Files::trusted_program(&path)));
        Files::let_others_change(&dir);
        assert!(refused(Files::trusted_program(&path)));
        assert!(refused(Files::trusted_program(&safe)));
    }

    // Unix mode bits and special file kinds have no Windows counterpart.
    #[cfg(unix)]
    #[test]
    fn trusted_program_requires_execute_bits_and_refuses_sticky_and_special_files() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let path = program(root.path(), "sidevoice.exe");
        for mode in [0o600, 0o644, 0o1700, 0o1777] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(refused(Files::trusted_program(&path)), "{mode:o}");
        }
        let _socket = imp::special_records(root.path());
        for name in ["fifo", "socket"] {
            assert!(refused(Files::trusted_program(&root.path().join(name))));
        }
    }

    // Windows aliases, native executable names and junctions have no Unix counterpart.
    #[cfg(windows)]
    #[test]
    fn trusted_program_refuses_windows_aliases_scripts_and_junctions() {
        let root = scratch();
        for name in [
            "CON.exe",
            "sidevoice.exe:payload",
            "sidevoice.exe.",
            "sidevoice.exe ",
        ] {
            assert_eq!(
                Files::trusted_program(&root.path().join(name))
                    .unwrap_err()
                    .kind(),
                ErrorKind::InvalidInput
            );
        }
        for name in [
            "sidevoice",
            "sidevoice.cmd",
            "sidevoice.bat",
            "sidevoice.ps1",
            "sidevoice.com",
        ] {
            let path = program(root.path(), name);
            assert!(refused(Files::trusted_program(&path)));
        }
        let dir = root.path().join("bin");
        Files::create_private_dir(&dir).unwrap();
        program(&dir, "sidevoice.exe");
        let link = root.path().join("junction");
        imp::link_dir(&dir, &link);
        assert!(refused(Files::trusted_program(&link.join("sidevoice.exe"))));
    }

    #[test]
    fn held_discovery_never_creates_missing_directories_or_repairs_access() {
        let root = scratch();
        let missing = root.path().join("missing").join("inside");
        assert_eq!(
            HeldDirectory::open(&missing, Access::Private)
                .err()
                .unwrap()
                .kind(),
            ErrorKind::NotFound
        );
        assert!(!root.path().join("missing").exists());
        assert_eq!(
            HeldDirectory::open(Path::new("relative"), Access::Private)
                .err()
                .unwrap()
                .kind(),
            ErrorKind::InvalidInput
        );
        // PathBuf::join normalizes `..` on a Windows verbatim path; preserve the spelling under test.
        let mut parent_component = root.path().as_os_str().to_os_string();
        parent_component.push(std::path::MAIN_SEPARATOR_STR);
        parent_component.push("..");
        assert_eq!(
            HeldDirectory::open(Path::new(&parent_component), Access::Private)
                .err()
                .unwrap()
                .kind(),
            ErrorKind::InvalidInput
        );
        let dir = root.path().join("data");
        Files::create_private_dir(&dir).unwrap();
        Files::let_others_read(&dir);
        assert_open_refused(&dir, Access::Private);
        let _held = held(&dir, Access::NotWritableByOthers);
        assert_eq!(others(&dir), Others::Read);
        Files::let_others_change(&dir);
        assert_open_refused(&dir, Access::NotWritableByOthers);
        assert_eq!(others(&dir), Others::Change);
    }

    #[test]
    fn held_records_are_bounded_private_atomic_and_removable() {
        let root = scratch();
        let path = root.path().join("data with spaces ü");
        Files::create_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        let name = OsStr::new("record ü.json");
        assert!(dir.read(name, Access::Private, 100).unwrap().is_none());
        dir.remove(name).unwrap();
        dir.write(name, b"first").unwrap();
        dir.write(name, b"second").unwrap();
        assert_eq!(
            dir.read(name, Access::Private, 6).unwrap().unwrap(),
            b"second"
        );
        assert_eq!(
            dir.read(name, Access::Private, 5).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        assert!(private(&path.join(name)));
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        dir.remove(name).unwrap();
        dir.remove(name).unwrap();
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn held_access_preserves_readable_parents_and_checks_each_records_access() {
        let root = scratch();
        let path = root.path().join("readable");
        Files::create_private_dir(&path).unwrap();
        Files::let_others_read(&path);
        let dir = held(&path, Access::NotWritableByOthers);
        let name = OsStr::new("record");
        dir.write(name, b"private").unwrap();
        assert!(private(&path.join(name)));
        Files::let_others_read(&path.join(name));
        assert!(refused(dir.read(name, Access::Private, 100)));
        assert_eq!(
            dir.read(name, Access::NotWritableByOthers, 100)
                .unwrap()
                .unwrap(),
            b"private"
        );
        dir.write(name, b"replacement").unwrap();
        assert!(private(&path.join(name)));
        assert_eq!(others(&path), Others::Read, "parent not repaired");
        Files::let_others_change(&path.join(name));
        assert!(refused(dir.read(name, Access::NotWritableByOthers, 100)));
        assert!(refused(dir.write(name, b"must not repair")));
        assert!(refused(dir.remove(name)));
        assert_eq!(fs::read(path.join(name)).unwrap(), b"replacement");
    }

    #[test]
    fn held_records_reject_links_hard_links_and_directories_without_touching_them() {
        let root = scratch();
        let path = root.path().join("data");
        Files::create_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        dir.write(OsStr::new("target"), b"untouched").unwrap();
        fs::hard_link(path.join("target"), path.join("hard")).unwrap();
        Files::symlink(path.join("target"), path.join("link"));
        Files::symlink(path.join("absent"), path.join("dangling"));
        Files::create_private_dir(&path.join("directory")).unwrap();
        for name in ["target", "hard", "link", "dangling", "directory"] {
            for access in [Access::Private, Access::NotWritableByOthers] {
                assert!(refused(dir.read(OsStr::new(name), access, 100)), "{name}");
            }
            assert!(refused(dir.write(OsStr::new(name), b"changed")), "{name}");
            assert!(refused(dir.remove(OsStr::new(name))), "{name}");
        }
        assert_eq!(fs::read(path.join("target")).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(&path).unwrap().count(), 5);
    }

    #[test]
    fn held_operations_reject_non_child_names() {
        let root = scratch();
        let path = root.path().join("data");
        Files::create_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        for name in [
            "",
            ".",
            "..",
            "../outside",
            "a/b",
            "a\\b",
            "record/",
            "nul\0byte",
        ] {
            let name = OsStr::new(name);
            assert_eq!(
                dir.read(name, Access::Private, 100).unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
            assert_eq!(
                dir.write(name, b"x").unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
            assert_eq!(
                dir.remove(name).unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
        }
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn held_directories_refuse_linked_or_unsafe_ancestry() {
        let root = scratch();
        let parent = root.path().join("parent");
        let path = parent.join("data");
        Files::ensure_private_dir(&path).unwrap();
        Files::symlink(&parent, root.path().join("link"));
        assert_open_refused(&root.path().join("link").join("data"), Access::Private);
        Files::symlink(&path, parent.join("leaf-link"));
        assert_open_refused(&parent.join("leaf-link"), Access::Private);
        Files::let_others_change(&parent);
        assert_open_refused(&path, Access::Private);
    }

    #[test]
    fn held_operations_recheck_permissions_changed_after_open() {
        for ancestor in [false, true] {
            let root = scratch();
            let parent = root.path().join("parent");
            let path = parent.join("data");
            Files::ensure_private_dir(&path).unwrap();
            let dir = held(&path, Access::Private);
            let name = OsStr::new("record");
            dir.write(name, b"untouched").unwrap();
            Files::let_others_change(if ancestor { &parent } else { &path });
            assert!(refused(dir.read(name, Access::Private, 100)));
            assert!(refused(dir.write(name, b"changed")));
            assert!(refused(dir.remove(name)));
            assert_eq!(fs::read(path.join(name)).unwrap(), b"untouched");
        }
    }

    #[test]
    fn held_ancestry_cannot_be_retargeted_by_renaming_any_directory() {
        // Unix refuses a changed identity; Windows prevents the rename until the capability is dropped.
        for ancestor in [false, true] {
            let root = scratch();
            let parent = root.path().join("parent");
            let path = parent.join("data");
            Files::ensure_private_dir(&path).unwrap();
            let dir = held(&path, Access::Private);
            let name = OsStr::new("record");
            dir.write(name, b"original").unwrap();
            let moving = if ancestor { &parent } else { &path };
            let displaced = root.path().join("displaced");
            if fs::rename(moving, &displaced).is_ok() {
                Files::ensure_private_dir(&path).unwrap();
                Files::write_private(&path.join(name), b"decoy").unwrap();
                assert!(refused(dir.read(name, Access::Private, 100)));
                assert!(refused(dir.write(name, b"changed")));
                assert!(refused(dir.remove(name)));
                assert_eq!(fs::read(path.join(name)).unwrap(), b"decoy");
                let original = if ancestor {
                    displaced.join("data")
                } else {
                    displaced
                };
                assert_eq!(fs::read(original.join(name)).unwrap(), b"original");
            } else {
                assert_eq!(
                    dir.read(name, Access::Private, 100).unwrap().unwrap(),
                    b"original"
                );
                drop(dir);
                fs::rename(moving, &displaced)
                    .expect("rename succeeds once held handles are released");
            }
        }
    }

    #[test]
    fn held_paths_support_long_spaced_and_non_ascii_names() {
        let root = scratch();
        let mut path = root.path().to_path_buf();
        for _ in 0..6 {
            path.push("a long directory name with spaces and ü characters");
        }
        Files::ensure_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        let name = OsStr::new("record 日本語.json");
        dir.write(name, b"record").unwrap();
        assert_eq!(
            dir.read(name, Access::Private, 100).unwrap().unwrap(),
            b"record"
        );
        dir.remove(name).unwrap();
    }

    // Only Windows distinguishes ordinary drive paths from extended-length verbatim paths.
    #[cfg(windows)]
    #[test]
    fn held_windows_long_paths_do_not_require_a_verbatim_prefix_from_the_caller() {
        let root = scratch();
        let mut extended = root.path().to_path_buf();
        for _ in 0..6 {
            extended.push("a long directory name with spaces and ü characters");
        }
        Files::ensure_private_dir(&extended).unwrap();
        // Fixture creation can use the extended path; every operation under test uses the ordinary drive path.
        let ordinary = Path::new(extended.to_str().unwrap().strip_prefix(r"\\?\").unwrap());
        assert!(ordinary.as_os_str().len() > 260);
        assert!(matches!(ordinary.components().next(),
            Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::Disk(_))));
        let dir = held(ordinary, Access::Private);
        let name = OsStr::new("record 日本語.json");
        dir.write(name, b"first").unwrap();
        dir.write(name, b"replacement").unwrap();
        assert_eq!(
            dir.read(name, Access::Private, 100).unwrap().unwrap(),
            b"replacement"
        );
        assert!(private(&ordinary.join(name)));
        assert_eq!(
            fs::read_dir(ordinary).unwrap().count(),
            1,
            "no staging files left"
        );
        dir.remove(name).unwrap();
        assert!(dir.read(name, Access::Private, 100).unwrap().is_none());
    }

    // Socket/FIFO fixtures are Unix filesystem object types; the assertions still use only Directory.
    #[cfg(unix)]
    #[test]
    fn held_special_records_are_permission_refusals_for_every_operation() {
        let root = scratch();
        let path = root.path().join("data");
        Files::create_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        let _socket = imp::special_records(&path);
        for name in ["socket", "fifo"] {
            let name = OsStr::new(name);
            for access in [Access::Private, Access::NotWritableByOthers] {
                assert!(refused(dir.read(name, access, 100)), "{name:?}");
            }
            assert!(refused(dir.write(name, b"must not replace")), "{name:?}");
            assert!(refused(dir.remove(name)), "{name:?}");
            assert!(
                fs::symlink_metadata(path.join(name)).is_ok(),
                "entry preserved"
            );
        }
        assert_eq!(fs::read_dir(&path).unwrap().count(), 2);
    }

    #[test]
    fn replacing_an_ancestor_is_detected_even_when_the_leaf_is_the_original_directory() {
        let root = scratch();
        let parent = root.path().join("parent");
        let path = parent.join("data");
        Files::ensure_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        let displaced = root.path().join("displaced");
        if fs::rename(&parent, &displaced).is_ok() {
            Files::create_private_dir(&parent).unwrap();
            fs::rename(displaced.join("data"), &path).unwrap();
            assert!(refused(dir.read(
                OsStr::new("record"),
                Access::Private,
                100
            )));
            assert!(refused(dir.write(OsStr::new("record"), b"x")));
            assert!(refused(dir.remove(OsStr::new("record"))));
            assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        } else {
            drop(dir);
            fs::rename(&parent, &displaced).unwrap();
        }
    }

    // Win32's path-based child operations rely on rename exclusion, unlike Unix's descriptor-relative operations.
    #[cfg(windows)]
    #[test]
    fn held_windows_handles_prevent_renaming_every_ancestor_until_drop() {
        let root = scratch();
        let parent = root.path().join("parent");
        let path = parent.join("data");
        Files::ensure_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        let displaced = root.path().join("displaced");
        assert!(
            fs::rename(&path, &displaced).is_err(),
            "leaf must stay bound"
        );
        assert!(
            fs::rename(&parent, &displaced).is_err(),
            "ancestor must stay bound"
        );
        drop(dir);
        fs::rename(&parent, &displaced).unwrap();
    }

    #[test]
    fn held_operations_never_reach_a_decoy_during_ancestor_rename_races() {
        let root = scratch();
        let parent = root.path().join("parent");
        let path = parent.join("data");
        Files::ensure_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        let decoy = root.path().join("decoy");
        Files::ensure_private_dir(&decoy.join("data")).unwrap();
        Files::write_private(&decoy.join("data/record"), b"decoy").unwrap();
        let displaced = root.path().join("displaced");
        std::thread::scope(|scope| {
            let renamer = scope.spawn(|| {
                for _ in 0..64 {
                    if fs::rename(&parent, &displaced).is_ok() {
                        fs::rename(&decoy, &parent).unwrap();
                        std::thread::yield_now();
                        fs::rename(&parent, &decoy).unwrap();
                        fs::rename(&displaced, &parent).unwrap();
                    }
                }
            });
            for _ in 0..64 {
                let name = OsStr::new("record");
                if let Err(error) = dir.write(name, b"original") {
                    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
                }
                match dir.read(name, Access::Private, 100) {
                    Ok(Some(bytes)) => assert_eq!(bytes, b"original"),
                    Ok(None) => (),
                    Err(error) => assert_eq!(error.kind(), ErrorKind::PermissionDenied),
                }
                if let Err(error) = dir.remove(name) {
                    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
                }
            }
            renamer.join().unwrap();
        });
        assert_eq!(fs::read(decoy.join("data/record")).unwrap(), b"decoy");
        assert_eq!(fs::read_dir(decoy.join("data")).unwrap().count(), 1);
    }

    // These aliases are a Win32 namespace property, not ordinary Unix filenames.
    #[cfg(windows)]
    #[test]
    fn held_windows_records_refuse_device_stream_and_trailing_aliases() {
        let root = scratch();
        let path = root.path().join("data");
        Files::create_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        for name in [
            "record:secret",
            "record.",
            "record ",
            "NUL",
            "con.txt",
            "COM1",
            "lpt².log",
            "CONIN$",
        ] {
            let name = OsStr::new(name);
            assert_eq!(
                dir.read(name, Access::Private, 100).unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
            assert_eq!(
                dir.write(name, b"x").unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
            assert_eq!(
                dir.remove(name).unwrap_err().kind(),
                ErrorKind::InvalidInput
            );
        }
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn held_concurrent_publication_never_exposes_partial_records() {
        let root = scratch();
        let path = root.path().join("data");
        Files::create_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        let name = OsStr::new("record");
        let first = vec![b'a'; 8192];
        let second = vec![b'b'; 8192];
        dir.write(name, &first).unwrap();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for _ in 0..24 {
                    // Windows may refuse replacement while another reader/antivirus holds the record.
                    if let Err(error) = dir.write(name, &second) {
                        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
                    }
                    if let Err(error) = dir.write(name, &first) {
                        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
                    }
                }
            });
            for _ in 0..48 {
                match dir.read(name, Access::Private, 8192) {
                    Ok(Some(bytes)) => assert!(bytes == first || bytes == second),
                    // An opened record replaced before its link-count check has no name, so it is refused.
                    Err(error) => assert_eq!(error.kind(), ErrorKind::PermissionDenied),
                    Ok(None) => panic!("atomic replacement must not leave the record absent"),
                }
            }
            writer.join().unwrap();
        });
        let bytes = dir.read(name, Access::Private, 8192).unwrap().unwrap();
        assert!(bytes == first || bytes == second);
        assert_eq!(
            fs::read_dir(&path).unwrap().count(),
            1,
            "no staging files left"
        );
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

    // The person's own home as the OS laid it out: on Windows `C:\Users\<name>` is owned by SYSTEM (or
    // Administrators) and grants its user full control.
    #[test]
    fn the_users_own_home_is_a_user_directory() {
        let home = dirs::home_dir().expect("a home directory");
        assert!(
            Files::check_user_directory(&home).is_ok(),
            "{}",
            home.display()
        );
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
        assert_open_refused(&dir, Access::Private);
        let _held = held(&dir, Access::NotWritableByOthers);
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
        assert_open_refused(&dir, Access::Private);
        let _held = held(&dir, Access::NotWritableByOthers);
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
        let _held = held(&dir, Access::Private);
        let _profile = held(&profile, Access::NotWritableByOthers);
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
        assert_open_refused(&link, Access::Private);
        assert_open_refused(&link.join("inside"), Access::Private);
        assert!(
            refused(Files::ensure_private_dir(&link.join("inside"))),
            "a link on the way is refused, not followed"
        );
    }
}
