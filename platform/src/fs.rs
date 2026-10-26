//! Private directories and files: the file-system half of the trust boundary, the OS user. It keeps out the other
//! users who are not administrators; root, this user and the administrators (Unix: the admin groups; Windows: SYSTEM,
//! Administrators, TrustedInstaller) are trusted. What keeps another user out is that every directory on the way to our
//! files belongs to somebody trusted and nobody else can replace what is in it, that our own directories and records
//! are open to nobody else and have no second name, and that a link planted in their place is never followed. A link
//! on the way to a directory is followed only where nobody untrusted can retarget it, and what it leads to is judged in
//! full (`resolve_links`). Anything that fails a check is refused (`PermissionDenied`), never repaired behind the
//! person's back and never used.
//!
//! The interface is the [`Fs`] trait; [`Files`] is the one OS implementation compiled in, chosen below. What differs
//! per OS is only how access is set and read: Unix owner, group and mode bits and `O_NOFOLLOW`; on Windows a protected
//! DACL set when a file or directory is created, read back from the handle (one that is not protected, or that passes
//! access down to others, is not private), and reparse points (links, junctions, mount points) refused at the path
//! itself. Who owns something is [`user`](crate::user)'s (`UserId::current().owns_path/owns_file`). Everything else has one
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

pub(crate) use imp::mount_of;
#[cfg(all(test, windows))]
pub(crate) use imp::set_junction;
#[cfg(unix)]
pub(crate) use imp::Mount;
#[cfg(windows)]
pub(crate) use imp::{open_relative, record_name};
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
    /// This user's, readable by others but never writable by them (Unix: an admin group or this user's own private
    /// group writing is not "others"). Directories may also belong to the system (Windows: as ordinary profile
    /// folders do; Unix: root or an administrator). Files must still belong to this user and have exactly one name.
    NotWritableByOthers,
}

/// Existing-only access through a checked directory and its held ancestry ([`HeldDirectory`]).
///
/// No operation creates a directory or repairs existing permissions. Links on the way to the directory (the directory
/// itself too) are followed as `resolve_links` allows and the directory they lead to is the one held; untrusted
/// links, links and reparse points at a record, unsafe ownership, hard-linked records and replaced ancestors are
/// refused with `PermissionDenied`. On Unix operations are relative
/// to the held directory; Windows holds every ancestor without delete sharing. Same-user/admin modification is
/// outside this boundary. Reads return `None` only for an absent child; an absent directory is `NotFound`.
///
/// Import both: `use sidevoice_platform::fs::{Directory, HeldDirectory, Access};`.
pub trait Directory: Sized {
    /// Opens an absolute existing directory, checking every ancestor. Missing directories remain missing. A link on
    /// the way or at the directory (Windows: a symlink, junction or mount point) is followed where trusted, as for
    /// [`Fs::ensure_private_dir`], and the directory it leads to is held. Parent (`..`) components are invalid.
    fn open(path: &Path, access: Access) -> io::Result<Self>;

    /// Opens an existing directory beneath this one without following any link: `relative` is descended one
    /// component at a time from the held handle, each step opened relative to the handle before it, never by path,
    /// with no link resolved (Unix: `openat` with `O_NOFOLLOW | O_DIRECTORY`; Windows: `NtCreateFile` with the
    /// parent handle as `RootDirectory` and `FILE_OPEN_REPARSE_POINT`). A link, junction or other reparse point at
    /// any component, a component on another device, volume or mount (a same-device bind mount included), and an
    /// unsafe directory on the way are refused with `PermissionDenied`; a missing one is `NotFound` and is never
    /// created. This directory may itself be a mount point: its own device and mount are the boundary. Use it to
    /// reach a directory that must be the one beneath an already held root, instead of checking a path and then
    /// opening it ([`Directory::open`] follows trusted links, so how this directory was selected is the caller's).
    ///
    /// `relative` is one or more ordinary child names joined by separators, each with the same syntax as a
    /// [`Directory::read`] name (Windows case and 8.3 aliases name the same child and are not refused); an empty,
    /// absolute or prefixed path, `.`, `..` and empty components are `InvalidInput`. `access` applies to the new
    /// leaf; every directory above it, this one included, must pass the ancestor rule. The result holds this
    /// directory's ancestry plus the new components and is verified before it is returned; later re-verification
    /// re-resolves paths and compares identities, which detects stable replacement but is not an atomic snapshot.
    /// It removes like one returned by [`Directory::open`]. This directory stays held and usable.
    fn open_beneath(&self, relative: &Path, access: Access) -> io::Result<Self>;

    /// Reads a regular record from the checked handle, bounded by `limit` (`InvalidData` when exceeded).
    /// Concurrent replacement can be refused if the opened record loses its name before the checks finish.
    /// `name` must be one ordinary child component, with no native separators, NUL, `.` or `..`.
    /// Backslash is ordinary on Unix; Windows also refuses device names, alternate data streams and trailing
    /// dot/space aliases. Invalid names return `InvalidInput`.
    fn read(&self, name: &OsStr, access: Access, limit: u64) -> io::Result<Option<Vec<u8>>>;

    /// Atomically publishes a private record through this directory, using the same staging/sync/publication as
    /// [`Fs::write_private`]. An existing record must satisfy `NotWritableByOthers`; unsafe state is never repaired.
    /// Ancestry is checked again after staging/sync, immediately before publication; on refusal the staged file
    /// is removed. Readable-but-not-writable-by-others directories are accepted. Windows has no directory flush
    /// guarantee.
    fn write(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()>;

    /// Removes one regular record satisfying `NotWritableByOthers`, never a directory or link. An absent child is
    /// success. The parent is synced where supported. Name validation is the same as for [`Directory::read`].
    fn remove(&self, name: &OsStr) -> io::Result<()>;
}

/// A root-to-leaf sequence, kept alive for the entire capability. Per-OS code controls how it is opened/reopened.
pub(crate) struct HeldState {
    chain: Vec<(PathBuf, File)>,
    // None applies the ancestor policy to the leaf too (a system or user program directory).
    access: Option<Access>,
}

impl HeldState {
    pub(crate) fn leaf(&self) -> (&Path, &File) {
        let (path, file) = self.chain.last().expect("an absolute directory has a root");
        (path, file)
    }

    // Called after the staged file is synced, immediately before publication. The writer retains responsibility
    // for removing the staging file if this check fails (relative to the held directory on Unix).
    fn before_publish(&self) -> io::Result<()> {
        #[cfg(test)]
        tests::before_publish();
        self.verify()
    }

    /// The chain [`Directory::open_beneath`] holds: this one's handles (duplicated), then each name of `relative`
    /// opened by `imp::open_subdirectory` relative to the handle before it, on this leaf's device and mount; verified
    /// whole. This leaf may itself be a mount point (it was opened explicitly); a mount met below it is refused.
    pub(crate) fn beneath(&self, relative: &Path, access: Access) -> io::Result<Self> {
        let names = relative_names(relative)?;
        self.verify()?;
        let start = self.leaf().1;
        let boundary = (Files::identity(start)?.volume, mount_of(start)?);
        let mut chain = self
            .chain
            .iter()
            .map(|(path, file)| Ok((path.clone(), file.try_clone()?)))
            .collect::<io::Result<Vec<_>>>()?;
        for (index, name) in names.iter().enumerate() {
            let leaf = index + 1 == names.len();
            let (parent_path, parent) = chain.last().expect("a held directory has a root");
            let path = parent_path.join(name);
            #[cfg(test)]
            tests::before_descent();
            let file = imp::open_subdirectory(parent, &path, !leaf)?;
            same_boundary(
                &boundary,
                &(Files::identity(&file)?.volume, mount_of(&file)?),
                &path,
            )?;
            if leaf {
                judge_access(&file, access, true).map_err(|error| refused(&path, error))?;
            } else {
                imp::check_ancestor_handle(&path, &file)?;
            }
            chain.push((path, file));
        }
        let held = HeldState {
            chain,
            access: Some(access),
        };
        held.verify()?;
        Ok(held)
    }

    pub(crate) fn verify(&self) -> io::Result<()> {
        for (index, (path, held)) in self.chain.iter().enumerate() {
            let leaf = index + 1 == self.chain.len();
            let current = imp::reopen_directory(path).map_err(|error| refused(path, error))?;
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

/// [`directory_paths`] of what `path` really names (`resolve_links`): `path` is validated as given, then resolved.
fn real_directory_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
    directory_paths(path)?;
    directory_paths(&resolve_links(path)?)
}

/// The path the absolute `path` really names, each link on the way followed only where nobody untrusted can retarget
/// it: the directory holding the link passes the ancestor rule (`imp::check_ancestor`), as does every directory above
/// it, and so does the link itself (`imp::check_link`). A link is a symbolic link, or on Windows also a junction or a
/// mount point; any other reparse point is left as it is, for the checks that follow to refuse. A root that only
/// stands for another directory (`imp::backing_directory`: a Windows `subst` drive) is replaced by that directory,
/// and a root whose directories cannot be checked (a network share) is refused. From the first missing component on,
/// the path is kept as written (created or reported missing later).
///
/// What is returned has no link on the way: callers hold and judge it in full, as any path, and work in it, so the
/// target is what is checked and what is used, never the link. A link is replaced only by whoever may write the
/// directory holding it, who is trusted, so the result names the same directory for as long as that holds.
fn resolve_links(path: &Path) -> io::Result<PathBuf> {
    // Links followed before giving up, as Linux's `MAXSYMLINKS`.
    const MAX_LINKS: usize = 40;
    let mut pending = parts(path);
    pending.reverse();
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop() {
        let name = match part {
            Part::Root(root) => {
                resolved.push(root);
                // A whole root that is another directory's alias (Windows: `subst`) is replaced by that directory,
                // whose every ancestor is then judged as any path's.
                if !matches!(pending.last(), Some(Part::Root(_))) {
                    if let Some(backing) = imp::backing_directory(&resolved)? {
                        links += 1;
                        if links > MAX_LINKS {
                            return Err(refused(path, "too many links on the way"));
                        }
                        resolved = PathBuf::new();
                        pending.extend(parts(&backing).into_iter().rev());
                    }
                }
                continue;
            }
            Part::Up => {
                resolved.pop();
                continue;
            }
            Part::Name(name) => name,
        };
        // Every directory entered is judged before what is in it is trusted, links included.
        imp::check_ancestor(&resolved)?;
        let next = resolved.join(&name);
        match fs::symlink_metadata(&next) {
            Ok(metadata) if metadata.is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(refused(path, "too many links on the way"));
                }
                imp::check_link(&next)?;
                let target = fs::read_link(&next)?;
                if target.is_absolute() {
                    resolved = PathBuf::new();
                }
                pending.extend(parts(&target).into_iter().rev());
            }
            Ok(_) => resolved = next,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                resolved = next;
                for part in pending.drain(..).rev() {
                    match part {
                        Part::Root(root) => resolved.push(root),
                        Part::Up => {
                            resolved.pop();
                        }
                        Part::Name(name) => resolved.push(name),
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(resolved)
}

/// One step of a path for `resolve_links`: its root (a Windows prefix, the root directory), `..`, or a name.
enum Part {
    Root(std::ffi::OsString),
    Up,
    Name(std::ffi::OsString),
}

fn parts(path: &Path) -> Vec<Part> {
    use std::path::Component;
    path.components()
        .filter_map(|component| match component {
            Component::Prefix(_) | Component::RootDir => {
                Some(Part::Root(component.as_os_str().to_owned()))
            }
            Component::CurDir => None,
            Component::ParentDir => Some(Part::Up),
            Component::Normal(name) => Some(Part::Name(name.to_owned())),
        })
        .collect()
}

/// Where `imp::write_whole` writes the record `path`: for a directory it judges ([`Publish::judged`]), in the one its
/// parent really names (`resolve_links`), the record's own name kept and never followed; as written otherwise.
fn record_path(path: &Path, how: Publish) -> io::Result<PathBuf> {
    if !how.judged() {
        return Ok(path.to_path_buf());
    }
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "a record must have a file name"))?;
    Ok(resolve_links(parent_of(path)?)?.join(name))
}

/// Whether a directory met in [`Directory::open_beneath`] is on the starting directory's device (Windows: volume)
/// and mount. A same-device bind mount has the same device but another mount: refused too.
fn same_boundary<M: PartialEq>(start: &(u64, M), here: &(u64, M), path: &Path) -> io::Result<()> {
    if start.0 != here.0 {
        return Err(refused(path, "on another device or volume"));
    }
    if start.1 != here.1 {
        return Err(refused(path, "on another mount"));
    }
    Ok(())
}

/// The names of `relative` for [`Directory::open_beneath`]: each one ordinary child name (on Windows also free of
/// the aliases `record_name` refuses). `Path::components` silently drops `.` and empty components after the first,
/// so their count is compared with the separator-delimited segments: whatever it normalized away is refused.
fn relative_names(relative: &Path) -> io::Result<Vec<&OsStr>> {
    let invalid = || {
        io::Error::new(
            ErrorKind::InvalidInput,
            "expected ordinary child names beneath the held directory",
        )
    };
    let mut names = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(invalid());
        };
        #[cfg(windows)]
        record_name(name)?;
        #[cfg(not(windows))]
        child_name(name)?;
        names.push(name);
    }
    let segments = relative
        .as_os_str()
        .as_encoded_bytes()
        .split(|&b| std::path::is_separator(char::from(b)))
        .count();
    if names.is_empty() || names.len() != segments {
        return Err(invalid());
    }
    Ok(names)
}

pub(crate) fn child_name(name: &OsStr) -> io::Result<&Path> {
    let path = Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
        || name
            .as_encoded_bytes()
            .iter()
            .any(|&b| b == 0 || std::path::is_separator(char::from(b)))
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

/// What somebody other than a file's owner (and the system: root, SYSTEM, Administrators, TrustedInstaller) may do with it.
/// On Unix an admin group or this user's own private group writing is `Read`, never `Change`: not private, not others.
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
    pub(crate) volume: u64,
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
    /// The regular file must belong to this user or the system (Unix root or an administrator; Windows SYSTEM,
    /// Administrators or TrustedInstaller), with no write/takeover rights granted to other users (Unix: an admin group
    /// or this user's private group may write). Its ancestry is held and checked with the same mechanism as
    /// [`Directory`], including system-owned program directories and Unix sticky parents.
    /// Unix requires an execute bit and rejects a sticky file. Windows accepts native `.exe` names, case-insensitively;
    /// it does not consult PATHEXT or invoke a shell. The OS loader still decides whether the payload can execute.
    /// Unlike private records, programs may have hard links: they confer no write rights or secret-file access.
    ///
    /// Supply an absolute path without `..`. Links (Windows: symlinks, junctions, mount points), the final one too,
    /// are resolved first where trusted, as for [`Fs::ensure_private_dir`], and the resolved ancestry and file are
    /// checked; launch the returned path, never the original link. Missing paths return `NotFound`,
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
    /// accept: this user or the system (Unix: root or an administrator; Windows: a profile folder `C:\Users\<name>` is
    /// owned by SYSTEM or Administrators and grants its user full control, the default layout).
    fn owner_of_user_directory_is_trusted(dir: &File) -> io::Result<bool>;

    /// Which file is behind an open handle (Unix: device and inode; Windows: volume serial number and the 128-bit
    /// file id, ReFS needing all of it; the 64-bit file index where the file system has no such id), read from the
    /// handle itself: the file being read, not whatever its path names a moment later.
    fn identity(file: &File) -> io::Result<FileId>;

    /// A private directory at the absolute `path`, made (with any missing parents, each private) when it does not
    /// exist, then checked with every directory on the way to it. The directory itself must be this user's and open
    /// to nobody else; each directory on the way owned by somebody trusted, with nobody else able to replace what is
    /// in it.
    ///
    /// A link on the way, or at the directory itself (one moved to another disk), is followed only when the directory
    /// holding it passes that rule, with everything above it, and so does the link: Unix, a link owned by root, this
    /// user or an administrator (its mode means nothing); Windows, a symlink, junction or mount point judged as a
    /// directory on the way is. Then what it leads to is judged in full, as any path; it is the directory checked.
    ///
    /// Unix judges each directory itself: owned by root, this user or an administrator (a member of an admin group),
    /// writable only by an admin group or this user's own private group, by anybody else only under the sticky bit
    /// (`/tmp`). Windows judges each directory: owned by this user, SYSTEM, Administrators or TrustedInstaller, nobody
    /// but this user and the system allowed to delete or rename what is in it or to take it over (a volume root, which
    /// cannot itself be deleted or renamed, does not count `DELETE` on it: `D:\` grants Authenticated Users Modify). An
    /// existing directory with the wrong owner or access is refused, not repaired.
    fn ensure_private_dir(path: &Path) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{} is not an absolute path", path.display()),
            ));
        }
        create_all::<Self>(path)?;
        // Every directory above what it really names is judged on the way there.
        let path = &resolve_links(path)?;
        let dir = Self::open_read(path).map_err(|error| refused(path, error))?;
        if !dir.metadata()?.is_dir() {
            return Err(refused(path, "not a directory"));
        }
        Self::check_private(&dir).map_err(|error| refused(path, error))
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
    thread_local! {
        static BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn before_publish() {
        let hook = BEFORE_PUBLISH.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    thread_local! {
        static BEFORE_DESCENT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    // Runs once, after the starting chain is verified and before the next component is opened: where a held
    // directory can change under a descent.
    pub(super) fn before_descent() {
        let hook = BEFORE_DESCENT.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

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
        let resolved = Files::trusted_program(&link).unwrap();
        assert_eq!(resolved, safe.canonicalize().unwrap());
        Files::trusted_program(&resolved).unwrap();
        fs::remove_file(&link).unwrap();
        Files::symlink(&unsafe_program, &link);
        assert!(refused(Files::trusted_program(&link)));
        // A link others could retarget is not followed, even to a safe program.
        let loose = root.path().join("loose");
        Files::create_private_dir(&loose).unwrap();
        Files::symlink(&safe, loose.join("sidevoice.exe"));
        Files::let_others_change(&loose);
        assert!(refused(Files::trusted_program(
            &loose.join("sidevoice.exe")
        )));
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
        assert_eq!(
            Files::trusted_program(&path).unwrap(),
            safe.canonicalize().unwrap()
        );
        Files::let_others_change(&dir);
        assert!(refused(Files::trusted_program(&path)));
        assert!(refused(Files::trusted_program(&safe)));
    }

    // Unix execute/search-only permissions have no equivalent mode bits on Windows. Copy a real native program:
    // a script interpreter would need to read a mode-0111 script even though the kernel can execute a binary.
    #[cfg(unix)]
    #[test]
    fn trusted_program_accepts_execute_only_binary_and_search_only_ancestry() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let dir = root.path().join("search-only");
        Files::create_private_dir(&dir).unwrap();
        let path = dir.join("sidevoice");
        fs::copy(imp::system_program(), &path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o111)).unwrap();
        let binary = Files::trusted_program(&path);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o111)).unwrap();
        let ancestry = Files::trusted_program(&path);
        let executes = std::process::Command::new(&path)
            .args(["-c", "exit 0"])
            .status();
        // Restore traversal for TempDir cleanup even when an assertion below fails.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(binary.unwrap(), path);
        assert_eq!(ancestry.unwrap(), path);
        assert!(executes.unwrap().success());
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
    fn trusted_program_refuses_windows_aliases_and_scripts_and_follows_trusted_junctions() {
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
        assert_eq!(
            Files::trusted_program(&link.join("sidevoice.exe")).unwrap(),
            dir.join("sidevoice.exe").canonicalize().unwrap()
        );
        let loose = root.path().join("loose");
        Files::create_private_dir(&loose).unwrap();
        imp::link_dir(&dir, &loose.join("junction"));
        Files::let_others_change(&loose);
        assert!(refused(Files::trusted_program(
            &loose.join("junction").join("sidevoice.exe")
        )));
    }

    // These spellings and ACLs are Windows-only; the assertions still use the public Fs interface.
    #[cfg(windows)]
    #[test]
    fn trusted_program_checks_real_short_name_aliases() {
        // The OS volume supports 8.3 names; explicitly assign aliases to our fixtures instead of trusting
        // GetShortPathName, which may silently return a long name when no short name exists.
        let root = tempfile::Builder::new()
            .prefix("sidevoice-p2-")
            .tempdir_in(std::env::var_os("ProgramFiles").unwrap())
            .unwrap();
        let dir = root.path().join("Sidevoice program directory");
        Files::create_private_dir(&dir).unwrap();
        let path = program(&dir, "sidevoice connector.exe");
        let file_alias = imp::short_name(&path, "SVPROG~1.EXE");
        let dir_alias = imp::short_name(&dir, "SVBIN~1");
        let both_aliases = dir_alias.join("SVPROG~1.EXE");
        let expected = Files::trusted_program(&path).unwrap();
        for alias in [&file_alias, &both_aliases] {
            assert_eq!(Files::trusted_program(alias).unwrap(), expected);
        }
        Files::let_others_change(&path);
        assert!(refused(Files::trusted_program(&both_aliases)));
        Files::shut_others_out(&path);
        Files::let_others_change(&dir);
        assert!(refused(Files::trusted_program(&both_aliases)));
        Files::shut_others_out(&dir);
        let script = program(&dir, "sidevoice.cmd");
        let disguised = imp::short_name(&script, "SCRIPT~1.EXE");
        assert!(
            refused(Files::trusted_program(&disguised)),
            "a native-looking alias must not return a script"
        );
    }

    #[cfg(windows)]
    #[test]
    fn trusted_program_checks_disk_namespaces_and_refuses_devices() {
        let root = scratch();
        let path = program(root.path(), "sidevoice.exe");
        let ordinary = imp::ordinary_path(&path);
        let verbatim = PathBuf::from(format!(r"\\?\{}", ordinary.display()));
        let device_disk = PathBuf::from(format!(r"\\.\{}", ordinary.display()));
        let expected = Files::trusted_program(&ordinary).unwrap();
        for alias in [&verbatim, &device_disk] {
            assert_eq!(Files::trusted_program(alias).unwrap(), expected);
        }
        Files::let_others_change(&path);
        for alias in [&verbatim, &device_disk] {
            assert!(refused(Files::trusted_program(alias)));
        }
        for device in [
            r"\\.\NUL.exe",
            r"\\?\GLOBALROOT\Device\Null.exe",
            r"\\.\pipe\sidevoice-invalid.exe",
        ] {
            assert!(Files::trusted_program(Path::new(device)).is_err());
        }
    }

    // A share's directories, even this machine's own, are reached through a server: what is above the shared
    // directory cannot be checked from the path, so nothing reached through a share is trusted.
    #[cfg(windows)]
    #[test]
    fn trusted_program_and_held_directories_refuse_unc_and_verbatim_unc_through_a_local_share() {
        let root = scratch();
        Files::shut_others_out(root.path());
        let dir = root.path().join("bin");
        Files::create_private_dir(&dir).unwrap();
        let path = program(&dir, "sidevoice.exe");
        Files::trusted_program(&path).unwrap();
        let share = imp::Share::new(root.path());
        let unc = share.path().join("bin").join("sidevoice.exe");
        let verbatim = PathBuf::from(format!(
            r"\\?\UNC\{}",
            unc.to_str().unwrap().trim_start_matches('\\')
        ));
        for alias in [&unc, &verbatim] {
            assert!(
                refused(Files::trusted_program(alias)),
                "{}",
                alias.display()
            );
            assert_open_refused(alias.parent().unwrap(), Access::Private);
            assert!(refused(Files::ensure_private_dir(
                &alias.parent().unwrap().join("data")
            )));
        }
    }

    #[cfg(windows)]
    #[test]
    fn trusted_program_checks_inherited_access_under_a_user_local_appdata_root() {
        let root = tempfile::Builder::new()
            .prefix("sidevoice-p2-")
            .tempdir_in(std::env::var_os("LOCALAPPDATA").unwrap())
            .unwrap();
        let dir = root.path().join("Sidevoice").join("install");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sidevoice.exe");
        fs::write(&path, b"native program fixture").unwrap(); // Inherit the user's ordinary profile DACL.
        Files::trusted_program(&path).unwrap();
        // A grant inherited only by child files leaves the directories unchanged but makes the payload unsafe.
        imp::icacls(root.path(), &["/grant", "*S-1-1-0:(OI)(IO)(M)"]);
        assert!(refused(Files::trusted_program(&path)));
    }

    #[cfg(windows)]
    #[test]
    fn trusted_program_accepts_system_installer_grants_on_a_user_owned_program() {
        let root = scratch();
        let path = program(root.path(), "sidevoice.exe");
        imp::icacls(&path, &["/grant", "NT SERVICE\\TrustedInstaller:(F)"]);
        Files::trusted_program(&path).unwrap();
        Files::let_others_change(&path);
        assert!(refused(Files::trusted_program(&path)));
    }

    #[cfg(windows)]
    #[test]
    fn trusted_program_refuses_another_users_program_files_subtree() {
        let other = imp::OtherUser::new();
        let root = tempfile::Builder::new()
            .prefix("sidevoice-p2-")
            .tempdir_in(std::env::var_os("ProgramFiles").unwrap())
            .unwrap();
        let path = program(root.path(), "sidevoice.exe");
        Files::trusted_program(&path).unwrap();
        imp::icacls(
            root.path(),
            &["/grant", &format!("{}:(OI)(CI)(F)", other.0)],
        );
        assert!(
            refused(Files::trusted_program(&path)),
            "another account can replace the program"
        );
        imp::icacls(root.path(), &["/remove:g", &other.0]);
        imp::icacls(root.path(), &["/setowner", &other.0]);
        assert!(
            refused(Files::trusted_program(&path)),
            "another account owns the program directory"
        );
        imp::icacls(root.path(), &["/setowner", &UserId::name().unwrap()]);
    }

    /// A private root holding the private directories `a/b`, and a private directory `outside` beside it with a
    /// record, for what a link beneath the root must never reach.
    fn beneath_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = scratch();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        for dir in [&root, &root.join("a"), &root.join("a").join("b"), &outside] {
            Files::create_private_dir(dir).unwrap();
        }
        Files::write_private(&outside.join("record"), b"outside").unwrap();
        (temp, root, outside)
    }

    fn beneath_kind(held: &HeldDirectory, relative: impl AsRef<Path>) -> ErrorKind {
        held.open_beneath(relative.as_ref(), Access::Private)
            .err()
            .unwrap()
            .kind()
    }

    #[test]
    fn open_beneath_descends_and_holds_the_whole_chain() {
        let (_temp, root, _) = beneath_fixture();
        let held = held(&root, Access::Private);
        let leaf = held
            .open_beneath(&Path::new("a").join("b"), Access::Private)
            .unwrap();
        let name = OsStr::new("record");
        leaf.write(name, b"inside").unwrap();
        assert_eq!(
            fs::read(root.join("a").join("b").join(name)).unwrap(),
            b"inside"
        );
        assert_eq!(
            leaf.read(name, Access::Private, 100).unwrap().unwrap(),
            b"inside"
        );
        // One component at a time gives the same directory; the root stays held and usable.
        let step = held
            .open_beneath(Path::new("a"), Access::Private)
            .and_then(|a| a.open_beneath(Path::new("b"), Access::Private))
            .unwrap();
        assert!(step.read(name, Access::Private, 100).unwrap().is_some());
        held.write(name, b"root").unwrap();
        // Missing components are reported, never created.
        assert_eq!(
            beneath_kind(&held, Path::new("a").join("missing").join("c")),
            ErrorKind::NotFound
        );
        assert!(!root.join("a").join("missing").exists());
        // Access applies to the leaf: a leaf others can read is not private.
        Files::let_others_read(&root.join("a").join("b"));
        assert_eq!(
            beneath_kind(&held, Path::new("a").join("b")),
            ErrorKind::PermissionDenied
        );
        held.open_beneath(&Path::new("a").join("b"), Access::NotWritableByOthers)
            .unwrap();
    }

    #[test]
    fn open_beneath_refuses_links_at_the_leaf_and_on_the_way() {
        let (_temp, root, outside) = beneath_fixture();
        Files::create_private_dir(&outside.join("sub")).unwrap();
        Files::symlink(&outside, root.join("leaf-link"));
        Files::symlink(&outside, root.join("a").join("link"));
        let held = held(&root, Access::Private);
        for relative in [
            Path::new("leaf-link").to_path_buf(),
            Path::new("leaf-link").join("sub"),
            Path::new("a").join("link"),
            Path::new("a").join("link").join("sub"),
        ] {
            assert_eq!(
                beneath_kind(&held, &relative),
                ErrorKind::PermissionDenied,
                "{relative:?}"
            );
        }
        assert_eq!(fs::read(outside.join("record")).unwrap(), b"outside");
    }

    // The desktop race: a directory checked by path is swapped for a link before it is opened. Descending from the
    // held root never reaches the link's target. (Windows refuses renaming a held directory; Unix shows the swap.)
    #[cfg(unix)]
    #[test]
    fn open_beneath_refuses_a_directory_swapped_for_a_link_and_a_held_chain_sees_the_swap() {
        let (temp, root, outside) = beneath_fixture();
        let held = held(&root, Access::Private);
        let b = held
            .open_beneath(&Path::new("a").join("b"), Access::Private)
            .unwrap();
        fs::rename(root.join("a"), temp.path().join("a-moved")).unwrap();
        Files::symlink(temp.path().join("a-moved"), root.join("a"));
        assert_eq!(
            beneath_kind(&held, Path::new("a").join("b")),
            ErrorKind::PermissionDenied
        );
        // The chain opened before the swap includes `a`: its next use is refused, not redirected.
        assert!(refused(b.write(OsStr::new("record"), b"redirected")));
        fs::remove_file(root.join("a")).unwrap();
        Files::symlink(&outside, root.join("a"));
        assert_eq!(
            beneath_kind(&held, Path::new("a")),
            ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(outside.join("record")).unwrap(), b"outside");
        assert!(!temp
            .path()
            .join("a-moved")
            .join("b")
            .join("record")
            .exists());
    }

    #[test]
    fn open_beneath_accepts_only_ordinary_child_names() {
        let (_temp, root, _) = beneath_fixture();
        let held = held(&root, Access::Private);
        let sep = std::path::MAIN_SEPARATOR_STR;
        let absolute = root.join("a");
        let mut invalid = vec![
            String::new(),
            ".".into(),
            "..".into(),
            format!("a{sep}.."),
            format!("a{sep}..{sep}a"),
            format!("a{sep}.{sep}b"),
            format!(".{sep}a"),
            format!("a{sep}{sep}b"),
            format!("a{sep}"),
            format!("{sep}a"),
            absolute.to_string_lossy().into_owned(),
        ];
        if cfg!(windows) {
            invalid.extend(["C:a", "a/./b", "a:stream", "CON", "a."].map(String::from));
        }
        for relative in invalid {
            assert_eq!(
                beneath_kind(&held, &relative),
                ErrorKind::InvalidInput,
                "{relative:?}"
            );
        }
    }

    // A mount point needs privileges to make; `/proc` is one under `/` wherever Linux runs the tests.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_beneath_refuses_another_device() {
        use std::os::unix::fs::MetadataExt;
        let root = held(Path::new("/"), Access::NotWritableByOthers);
        let proc = fs::metadata("/proc").unwrap();
        assert_ne!(
            proc.dev(),
            fs::metadata("/").unwrap().dev(),
            "/proc is not a mount"
        );
        let error = root
            .open_beneath(Path::new("proc"), Access::NotWritableByOthers)
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("another device"), "{error}");
    }

    // The decision behind descent's boundary: a same-device bind mount differs only in its mount.
    #[test]
    fn descent_boundary_refuses_another_device_and_another_mount_on_the_same_device() {
        let path = Path::new("component");
        assert!(same_boundary(&(1, 10), &(1, 10), path).is_ok());
        let device = same_boundary(&(1, 10), &(2, 10), path).unwrap_err();
        assert!(device.to_string().contains("another device"), "{device}");
        let mount = same_boundary(&(1, 10), &(1, 11), path).unwrap_err();
        assert_eq!(mount.kind(), ErrorKind::PermissionDenied);
        assert!(mount.to_string().contains("another mount"), "{mount}");
    }

    /// Directory mount points on the same device as the mount holding their parent (bind mounts), from
    /// `/proc/self/mountinfo` (field 3: `major:minor`, field 5: the mount point; `\040` and the like left escaped).
    #[cfg(target_os = "linux")]
    fn same_device_bind_mounts() -> Vec<PathBuf> {
        let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
        let mounts: Vec<(&str, PathBuf)> = info
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split(' ').collect();
                Some((*fields.get(2)?, PathBuf::from(fields.get(4)?)))
            })
            .collect();
        let device_of = |path: &Path| {
            mounts
                .iter()
                .filter(|(_, point)| path.starts_with(point))
                .max_by_key(|(_, point)| point.components().count())
                .map(|(device, _)| *device)
        };
        mounts
            .iter()
            .filter(|(device, point)| {
                point
                    .parent()
                    .is_some_and(|parent| device_of(parent) == Some(*device))
                    && fs::symlink_metadata(point).is_ok_and(|m| m.is_dir())
            })
            .map(|(_, point)| point.clone())
            .collect()
    }

    // Making a bind mount needs privileges the tests do not have; where the host already has one (a container's
    // read-only `/proc/sys`, for one) it is used, otherwise the decision test above stands for it.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_beneath_refuses_a_same_device_bind_mount_but_not_a_mounted_start() {
        let mut checked = false;
        let binds = same_device_bind_mounts();
        for point in &binds {
            let (Some(parent), Some(name)) = (point.parent(), point.file_name()) else {
                continue;
            };
            let Ok(above) = HeldDirectory::open(parent, Access::NotWritableByOthers) else {
                continue;
            };
            let error = above
                .open_beneath(Path::new(name), Access::NotWritableByOthers)
                .err()
                .unwrap_or_else(|| panic!("{}: a bind mount was descended into", point.display()));
            assert!(
                error.to_string().contains("another mount"),
                "{}: {error}",
                point.display()
            );
            // The mount opened explicitly is a valid start; what is below it, on the same mount, is descended.
            if let Ok(start) = HeldDirectory::open(point, Access::NotWritableByOthers) {
                let inner = fs::read_dir(point)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .find(|entry| {
                        entry.file_type().is_ok_and(|kind| kind.is_dir())
                            && !binds.contains(&entry.path())
                    });
                if let Some(inner) = inner {
                    let below = start
                        .open_beneath(Path::new(&inner.file_name()), Access::NotWritableByOthers);
                    assert!(
                        below
                            .as_ref()
                            .map_or_else(|e| !e.to_string().contains("mount"), |_| true),
                        "{}: {:?}",
                        inner.path().display(),
                        below.err()
                    );
                }
            }
            checked = true;
            break;
        }
        if !checked {
            eprintln!("no reachable same-device bind mount on this host: covered by the boundary decision test");
        }
    }

    // The P1 race on Windows: an empty held directory becomes a junction in place after the chain was verified and
    // before the next component is opened. The child is opened relative to the held handle, never by path, so the
    // junction's target is never reached. Native NTFS refuses that relative open with `ERROR_CANT_RESOLVE_FILENAME`
    // (1921, `FilesystemLoop`): the parse starts at a directory that is now a reparse point and is not resolved.
    // `NotFound` (the child looked up in the empty held directory itself) is the other justified refusal; any other
    // error, or success, fails the test.
    #[cfg(windows)]
    #[test]
    fn open_beneath_never_traverses_a_junction_set_on_a_held_directory() {
        use std::os::windows::fs::MetadataExt;
        let temp = scratch();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        for dir in [&root, &outside, &outside.join("c")] {
            Files::create_private_dir(dir).unwrap();
        }
        let held = held(&root, Access::Private);
        let (junction, target) = (root.clone(), outside.clone());
        BEFORE_DESCENT.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || set_junction(&junction, &target)));
        });
        let error = held
            .open_beneath(Path::new("c"), Access::Private)
            .err()
            .unwrap();
        assert!(
            error.kind() == ErrorKind::NotFound || error.raw_os_error() == Some(1921),
            "{error:?}"
        );
        assert!(fs::symlink_metadata(&root).unwrap().file_attributes() & 0x400 != 0);
        // The capability's next use sees the junction and refuses.
        assert!(refused(held.write(OsStr::new("record"), b"redirected")));
        assert!(!outside.join("record").exists());
        assert!(fs::read_dir(outside.join("c")).unwrap().next().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn open_beneath_refuses_junctions() {
        let (_temp, root, outside) = beneath_fixture();
        Files::create_private_dir(&outside.join("sub")).unwrap();
        for junction in [root.join("junction"), root.join("a").join("junction")] {
            assert!(std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&junction)
                .arg(&outside)
                .output()
                .unwrap()
                .status
                .success());
        }
        let held = held(&root, Access::Private);
        for relative in [
            Path::new("junction").to_path_buf(),
            Path::new("junction").join("sub"),
            Path::new("a").join("junction"),
            Path::new("a").join("junction").join("sub"),
        ] {
            assert_eq!(
                beneath_kind(&held, &relative),
                ErrorKind::PermissionDenied,
                "{relative:?}"
            );
        }
        assert_eq!(fs::read(outside.join("record")).unwrap(), b"outside");
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

    // Unix search-only permissions have no equivalent mode bits on Windows.
    #[cfg(unix)]
    #[test]
    fn held_records_work_below_search_only_ancestors_without_repairing_them() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let above = root.path().join("search-only");
        Files::create_private_dir(&above).unwrap();
        let path = above.join("data");
        Files::create_private_dir(&path).unwrap();
        let name = OsStr::new("record");
        for mode in [0o311, 0o711] {
            for access in [Access::Private, Access::NotWritableByOthers] {
                Files::write_private(&path.join(name), b"existing").unwrap();
                fs::set_permissions(&above, fs::Permissions::from_mode(mode)).unwrap();
                let operations = (|| -> io::Result<_> {
                    let dir = HeldDirectory::open(&path, access)?;
                    let before = dir.read(name, Access::Private, 100)?;
                    dir.write(name, b"replacement")?;
                    let after = dir.read(name, Access::Private, 100)?;
                    dir.remove(name)?;
                    Ok((before, after, dir.read(name, Access::Private, 100)?))
                })();
                let unchanged = fs::metadata(&above).unwrap().permissions().mode() & 0o777;
                // Restore listing access for TempDir cleanup even if the operation failed.
                fs::set_permissions(&above, fs::Permissions::from_mode(0o700)).unwrap();
                let (before, after, removed) = operations.unwrap();
                assert_eq!(before.as_deref(), Some(b"existing".as_slice()));
                assert_eq!(after.as_deref(), Some(b"replacement".as_slice()));
                assert!(removed.is_none());
                assert_eq!(unchanged, mode, "ancestor permissions must not be repaired");
            }
        }
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
        for name in ["", ".", "..", "../outside", "a/b", "record/", "nul\0byte"] {
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

    #[cfg(unix)]
    #[test]
    fn held_unix_records_accept_ordinary_unix_filename_bytes() {
        let root = scratch();
        let path = root.path().join("data");
        Files::create_private_dir(&path).unwrap();
        let dir = held(&path, Access::Private);
        for name in [
            r"session\backup",
            r"\",
            r"..\backup",
            r"backup\",
            "record:stream",
            "NUL",
            "record.",
            "record ",
        ] {
            let name = OsStr::new(name);
            dir.write(name, b"literal name").unwrap();
            assert_eq!(
                dir.read(name, Access::Private, 100).unwrap().unwrap(),
                b"literal name"
            );
            assert_eq!(fs::read(path.join(name)).unwrap(), b"literal name");
            dir.remove(name).unwrap();
            assert!(!path.join(name).exists());
        }
    }

    #[test]
    fn held_directories_follow_trusted_links_and_refuse_unsafe_ancestry() {
        let root = scratch();
        let parent = root.path().join("parent");
        let path = parent.join("data");
        Files::ensure_private_dir(&path).unwrap();
        Files::symlink(&parent, root.path().join("link"));
        held(&root.path().join("link").join("data"), Access::Private)
            .write(OsStr::new("through-ancestor"), b"1")
            .unwrap();
        Files::symlink(&path, parent.join("leaf-link"));
        held(&parent.join("leaf-link"), Access::Private)
            .write(OsStr::new("through-leaf"), b"2")
            .unwrap();
        assert_eq!(fs::read(path.join("through-ancestor")).unwrap(), b"1");
        assert_eq!(fs::read(path.join("through-leaf")).unwrap(), b"2");
        // A link others could retarget is not followed, wherever it leads.
        let loose = root.path().join("loose");
        Files::create_private_dir(&loose).unwrap();
        Files::symlink(&path, loose.join("link"));
        Files::let_others_change(&loose);
        assert_open_refused(&loose.join("link"), Access::Private);
        Files::let_others_change(&parent);
        assert_open_refused(&path, Access::Private);
        assert_open_refused(&root.path().join("link").join("data"), Access::Private);
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
    fn held_write_rechecks_ancestry_after_staging_before_publication() {
        for ancestor in [false, true] {
            let root = scratch();
            let parent = root.path().join("parent");
            let path = parent.join("data");
            Files::ensure_private_dir(&path).unwrap();
            let dir = held(&path, Access::Private);
            dir.write(OsStr::new("record"), b"original").unwrap();
            let moving = if ancestor {
                parent.clone()
            } else {
                path.clone()
            };
            let displaced = root.path().join("displaced");
            let hook_path = path.clone();
            let hook_moving = moving.clone();
            let hook_displaced = displaced.clone();
            BEFORE_PUBLISH.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    assert_eq!(fs::read(hook_path.join("record")).unwrap(), b"original");
                    let staged = fs::read_dir(&hook_path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path())
                        .find(|entry| {
                            is_staging_name(OsStr::new("record"), entry.file_name().unwrap())
                        })
                        .expect("hook runs after staging, before publication");
                    assert_eq!(fs::read(staged).unwrap(), b"replacement");
                    let renamed = fs::rename(&hook_moving, &hook_displaced);
                    if cfg!(windows) {
                        assert!(
                            renamed.is_err(),
                            "held Windows ancestry must prevent replacement"
                        );
                    } else {
                        renamed.unwrap();
                        Files::ensure_private_dir(&hook_path).unwrap();
                        Files::write_private(&hook_path.join("record"), b"decoy").unwrap();
                    }
                }));
            });
            let result = dir.write(OsStr::new("record"), b"replacement");
            assert!(
                BEFORE_PUBLISH.with(|slot| slot.borrow().is_none()),
                "hook ran"
            );
            if cfg!(windows) {
                result.unwrap();
                assert_eq!(fs::read(path.join("record")).unwrap(), b"replacement");
                assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
                drop(dir);
                fs::rename(&moving, &displaced).expect("rename works after held handles close");
            } else {
                assert!(refused(result));
                let original = if ancestor {
                    displaced.join("data")
                } else {
                    displaced
                };
                assert_eq!(fs::read(original.join("record")).unwrap(), b"original");
                assert_eq!(
                    fs::read_dir(original).unwrap().count(),
                    1,
                    "staging file removed from displaced directory"
                );
                assert_eq!(fs::read(path.join("record")).unwrap(), b"decoy");
                assert_eq!(
                    fs::read_dir(path).unwrap().count(),
                    1,
                    "replacement directory untouched"
                );
            }
        }
    }

    #[test]
    fn held_write_refuses_post_staging_permission_changes_and_cleans_up() {
        // Windows prevents ancestor renames; changed access exercises refusal and staging cleanup on every OS.
        for ancestor in [false, true] {
            let root = scratch();
            let parent = root.path().join("parent");
            let path = parent.join("data");
            Files::ensure_private_dir(&path).unwrap();
            let dir = held(&path, Access::Private);
            dir.write(OsStr::new("record"), b"original").unwrap();
            let changed = if ancestor { parent } else { path.clone() };
            let hook_path = path.clone();
            BEFORE_PUBLISH.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    assert_eq!(
                        fs::read_dir(&hook_path).unwrap().count(),
                        2,
                        "record and staged file"
                    );
                    Files::let_others_change(&changed);
                }));
            });
            assert!(refused(dir.write(OsStr::new("record"), b"replacement")));
            assert!(
                BEFORE_PUBLISH.with(|slot| slot.borrow().is_none()),
                "hook ran"
            );
            assert_eq!(fs::read(path.join("record")).unwrap(), b"original");
            assert_eq!(
                fs::read_dir(path).unwrap().count(),
                1,
                "staging file removed"
            );
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
            r"nested\child",
            r"\record",
            r"record\",
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
    fn a_file_is_not_a_private_directory_and_a_link_is_judged_by_where_it_leads() {
        let root = scratch();
        let file = root.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(Files::ensure_private_dir(&file).is_err());
        let target = root.path().join("target");
        Files::ensure_private_dir(&target).unwrap();
        // A data directory moved elsewhere, a link of this user's left in its place.
        let link = root.path().join("link");
        Files::symlink(&target, &link);
        Files::ensure_private_dir(&link).unwrap();
        Files::let_others_read(&target);
        assert!(
            refused(Files::ensure_private_dir(&link)),
            "the directory it leads to must be private"
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
    fn a_link_on_the_way_is_followed_only_where_nobody_else_can_retarget_it() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::ensure_private_dir(&dir).unwrap();
        let link = root.path().join("link");
        Files::symlink(&dir, &link);
        Files::create_private_dir(&dir.join("inside")).unwrap();
        Files::ensure_private_dir(&link.join("inside")).unwrap();
        Files::ensure_private_dir(&link.join("made")).unwrap();
        assert!(private(&dir.join("made")), "made where it leads");
        // The same link in a directory others can change: they could point it anywhere.
        let loose = root.path().join("loose");
        Files::create_private_dir(&loose).unwrap();
        Files::symlink(&dir, loose.join("link"));
        Files::let_others_change(&loose);
        assert!(refused(Files::ensure_private_dir(
            &loose.join("link").join("inside")
        )));
        // A trusted link into a directory others can change leads to an unsafe ancestry.
        Files::let_others_change(&dir);
        assert!(refused(Files::ensure_private_dir(&link.join("inside"))));
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
        // Nor through a link others could retarget; through a trusted one, in the directory it leads to.
        Files::shut_others_out(&open);
        let loose = root.path().join("loose");
        Files::create_private_dir(&loose).unwrap();
        Files::symlink(&open, loose.join("link"));
        Files::let_others_change(&loose);
        assert!(refused(Files::open_created_file(
            &loose.join("link").join("identity.json")
        )));
        assert!(open.join(".identity.json.j1K2l3").exists());
        let link = root.path().join("link");
        Files::symlink(&open, &link);
        Files::open_created_file(&link.join("identity.json")).unwrap();
        assert!(!open.join(".identity.json.j1K2l3").exists());
    }

    // Unix only: group write bits have no Windows counterpart (grants to Administrators are judged by the Windows tests).
    // Each kind of group is tried where this host makes this user a member of one: this user's private group (most
    // Linux), an admin group (a macOS administrator), any other (macOS `staff`, a shared Linux group).
    #[cfg(unix)]
    #[test]
    fn group_write_is_trusted_only_from_an_admin_group_or_this_users_private_group() {
        use std::os::unix::fs::{chown, PermissionsExt};
        let mode = |path: &Path, mode| {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap()
        };
        let groups = imp::test_groups();
        for (gid, trusted) in [
            (groups.admin, true),
            (groups.private, true),
            (groups.other, false),
        ] {
            let Some(gid) = gid else { continue };
            let root = scratch();
            let shared = root.path().join("shared");
            Files::create_private_dir(&shared).unwrap();
            let path = program(&shared, "sidevoice");
            chown(&shared, None, Some(gid)).unwrap();
            mode(&shared, 0o2775);
            // On the way, as the checked directory, and above a program.
            assert_eq!(
                Files::ensure_private_dir(&shared.join("data")).is_ok(),
                trusted,
                "{gid}"
            );
            assert_eq!(
                HeldDirectory::open(&shared, Access::NotWritableByOthers).is_ok(),
                trusted,
                "{gid}"
            );
            assert_eq!(Files::trusted_program(&path).is_ok(), trusted, "{gid}");
            assert_open_refused(&shared, Access::Private);
            // The program itself.
            mode(&shared, 0o700);
            chown(&path, None, Some(gid)).unwrap();
            mode(&path, 0o770);
            assert_eq!(Files::trusted_program(&path).is_ok(), trusted, "{gid}");
            // Everybody writing is refused whatever the group, as before.
            mode(&shared, 0o777);
            assert!(refused(Files::ensure_private_dir(&shared.join("data"))));
        }
    }

    // Unix only: an extended ACL beside the mode bits is Unix's (a Windows DACL is the whole of its access, judged in
    // every test). Its grants to another user or group are judged as the mode's are, whatever the bits show.
    #[cfg(unix)]
    #[test]
    fn an_acl_letting_another_user_or_group_write_is_refused_and_one_letting_them_read_is_not() {
        for whom in [imp::Grantee::User, imp::Grantee::Group] {
            let root = scratch();
            let dir = |name: &str| {
                let dir = root.path().join(name);
                Files::create_private_dir(&dir).unwrap();
                dir
            };
            // Reading: not a writer, but not private either.
            let reading = dir("reading");
            let path = program(&reading, "sidevoice");
            imp::grant_by_acl(&reading, whom, false);
            imp::grant_by_acl(&path, whom, false);
            assert_eq!(others(&path), Others::Read, "{whom:?}");
            assert!(Files::trusted_program(&path).is_ok(), "{whom:?}");
            Files::ensure_private_dir(&reading.join("data")).unwrap();
            let _held = held(&reading, Access::NotWritableByOthers);
            assert_open_refused(&reading, Access::Private);
            // Writing the program.
            let program_dir = dir("program");
            let path = program(&program_dir, "sidevoice");
            imp::grant_by_acl(&path, whom, true);
            assert_eq!(others(&path), Others::Change, "{whom:?}");
            assert!(refused(Files::trusted_program(&path)), "{whom:?}");
            // Writing a directory on the way, above a private program, directory and record.
            let ancestor = dir("ancestor");
            let path = program(&ancestor, "sidevoice");
            Files::ensure_private_dir(&ancestor.join("data")).unwrap();
            imp::grant_by_acl(&ancestor, whom, true);
            assert!(refused(Files::trusted_program(&path)), "{whom:?}");
            assert!(refused(Files::ensure_private_dir(&ancestor.join("data"))));
            assert_open_refused(&ancestor.join("data"), Access::Private);
            assert_open_refused(&ancestor, Access::NotWritableByOthers);
            // Writing the directory that holds a link on the way: it could retarget it.
            let target = dir("target");
            let holder = dir("holder");
            Files::symlink(&target, holder.join("link"));
            Files::ensure_private_dir(&holder.join("link").join("inside")).unwrap();
            imp::grant_by_acl(&holder, whom, true);
            assert!(refused(Files::ensure_private_dir(
                &holder.join("link").join("inside")
            )));
            assert_open_refused(&holder.join("link"), Access::Private);
        }
    }

    // Unix only, as the test above. An ACL that passes access down to what is created in a directory (Linux: a default
    // ACL; macOS: `file_inherit`) must never reach a published record: on Linux the mode a record is created with
    // bounds it, and the record is private; on macOS it would not be, so nothing is staged there at all. Either way, no
    // record anybody else may read or write, new or replacing one, is ever published.
    #[cfg(unix)]
    #[test]
    fn an_acl_passing_access_down_never_reaches_a_published_record() {
        for whom in [imp::Grantee::User, imp::Grantee::Group] {
            for write in [false, true] {
                let root = scratch();
                let dir = root.path().join("passing-down");
                Files::create_private_dir(&dir).unwrap();
                let held = held(&dir, Access::NotWritableByOthers);
                held.write(OsStr::new("existing"), b"before").unwrap();
                imp::pass_down_by_acl(&dir, whom, write);
                for name in ["existing", "new"] {
                    match held.write(OsStr::new(name), b"secret") {
                        Ok(()) => assert!(private(&dir.join(name)), "{whom:?} {write} {name}"),
                        Err(error) => assert_eq!(error.kind(), ErrorKind::PermissionDenied),
                    }
                    match Files::write_private(&dir.join(name), b"secret") {
                        Ok(()) => assert!(private(&dir.join(name)), "{whom:?} {write} {name}"),
                        Err(error) => assert_eq!(error.kind(), ErrorKind::PermissionDenied),
                    }
                }
                assert!(private(&dir.join("existing")), "{whom:?} {write}");
                let mut names: Vec<_> = fs::read_dir(&dir)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                names.sort();
                assert!(
                    names == ["existing"] || names == ["existing", "new"],
                    "nothing staged is left: {names:?}"
                );
                #[cfg(target_os = "macos")]
                assert_eq!(
                    fs::read(dir.join("existing")).unwrap(),
                    b"before",
                    "refused before staging"
                );
            }
        }
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
    fn a_private_write_follows_only_a_link_nobody_else_can_retarget() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::create_private_dir(&dir).unwrap();
        let loose = root.path().join("loose");
        Files::create_private_dir(&loose).unwrap();
        Files::symlink(&dir, loose.join("link"));
        Files::let_others_change(&loose);
        let untrusted = loose.join("link");
        assert!(refused(Files::write_private(
            &untrusted.join("state.json"),
            b"secret"
        )));
        assert!(refused(Files::create_private(
            &untrusted.join("identity.json"),
            b"secret"
        )));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "nothing written");
        let link = root.path().join("link");
        Files::symlink(&dir, &link);
        Files::write_private(&link.join("state.json"), b"secret").unwrap();
        Files::create_private(&link.join("identity.json"), b"made").unwrap();
        assert_eq!(fs::read(dir.join("state.json")).unwrap(), b"secret");
        assert!(
            private(&dir.join("identity.json")),
            "written where it leads"
        );
        // What it leads to is still judged.
        Files::let_others_read(&dir);
        assert!(refused(Files::write_private(
            &link.join("state.json"),
            b"later"
        )));
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
    fn a_junction_on_the_way_is_followed_only_where_nobody_else_can_retarget_it() {
        let root = scratch();
        let dir = root.path().join("private");
        Files::ensure_private_dir(&dir).unwrap();
        // A profile folder moved to another disk, a junction left in its place.
        let link = root.path().join("link");
        imp::link_dir(&dir, &link);
        Files::ensure_private_dir(&link.join("inside")).unwrap();
        assert!(private(&dir.join("inside")), "made where it leads");
        held(&link, Access::Private)
            .write(OsStr::new("record"), b"through")
            .unwrap();
        assert_eq!(fs::read(dir.join("record")).unwrap(), b"through");
        // In a directory others can change, or itself changeable by others, it could be pointed anywhere.
        let loose = root.path().join("loose");
        Files::create_private_dir(&loose).unwrap();
        imp::link_dir(&dir, &loose.join("link"));
        Files::let_others_change(&loose);
        assert_open_refused(&loose.join("link"), Access::Private);
        assert!(refused(Files::ensure_private_dir(
            &loose.join("link").join("inside")
        )));
        let open_link = root.path().join("open-link");
        imp::link_dir(&dir, &open_link);
        imp::icacls(&open_link, &["/grant", "*S-1-1-0:(F)", "/L"]);
        assert_open_refused(&open_link, Access::Private);
    }

    // Windows only: a volume root is a Windows notion (Unix has one root, owned by root). Only a local fixed volume's
    // root is one: `subst` gives a directory a drive letter, but it stays a directory others may rename through its
    // real path, so Modify on it is refused under either name.
    #[cfg(windows)]
    #[test]
    fn only_a_local_fixed_volume_root_may_let_others_modify_it() {
        let root = scratch();
        let mut system = std::env::var_os("SystemDrive").unwrap();
        system.push(r"\");
        let system = PathBuf::from(system);
        assert!(imp::is_local_volume_root(
            &imp::reopen_directory(&system).unwrap()
        ));
        let volume = root.path().join("volume");
        fs::create_dir(&volume).unwrap();
        assert!(!imp::is_local_volume_root(
            &imp::reopen_directory(&volume).unwrap()
        ));
        // Authenticated Users: Modify, on the directory and passed down to what is made below it.
        imp::icacls(&volume, &["/grant", "*S-1-5-11:(OI)(CI)(M)"]);
        let drive = imp::Drive::new(&volume);
        assert!(!imp::is_local_volume_root(
            &imp::reopen_directory(&drive.root()).unwrap()
        ));
        assert!(refused(Files::ensure_private_dir(
            &drive.root().join("Sidevoice")
        )));
        assert!(refused(Files::ensure_private_dir(
            &volume.join("Sidevoice")
        )));
        // Without the grant, the aliased directory is judged as any other and passes.
        imp::icacls(&volume, &["/remove:g", "*S-1-5-11"]);
        let data = drive.root().join("Sidevoice");
        Files::ensure_private_dir(&data).unwrap();
        let _held = held(&data, Access::Private);
    }

    // Windows only, as above. A `subst` drive's directory grants nobody else anything, but the directory above it may
    // let others delete what is in it (`FILE_DELETE_CHILD`): they could rename the drive's directory away and put
    // their own in its place. What is reached through the drive is judged through its real path, that parent too.
    #[cfg(windows)]
    #[test]
    fn a_subst_drive_is_refused_when_others_can_replace_the_directory_behind_it() {
        let root = scratch();
        let shared = root.path().join("shared");
        Files::create_private_dir(&shared).unwrap();
        let volume = shared.join("volume");
        Files::create_private_dir(&volume).unwrap();
        let bin = volume.join("bin");
        Files::create_private_dir(&bin).unwrap();
        let path = program(&bin, "sidevoice.exe");
        Files::ensure_private_dir(&volume.join("data")).unwrap();
        let drive = imp::Drive::new(&volume);
        let aliased = |path: &Path| drive.root().join(path.strip_prefix(&volume).unwrap());
        // Authenticated Users: delete child, on the parent only.
        imp::icacls(&shared, &["/grant", "*S-1-5-11:(DC)"]);
        assert_eq!(others(&volume), Others::Nothing, "nothing granted on it");
        assert!(refused(Files::trusted_program(&aliased(&path))));
        assert!(refused(Files::ensure_private_dir(&aliased(
            &volume.join("data")
        ))));
        assert_open_refused(&drive.root(), Access::NotWritableByOthers);
        assert_open_refused(&aliased(&volume.join("data")), Access::Private);
        // Once nobody else can, the drive leads to the real path, which is what is held and returned.
        imp::icacls(&shared, &["/remove:g", "*S-1-5-11"]);
        assert_eq!(
            Files::trusted_program(&aliased(&path)).unwrap(),
            path.canonicalize().unwrap()
        );
        held(&aliased(&volume.join("data")), Access::Private)
            .write(OsStr::new("record"), b"through the drive")
            .unwrap();
        assert_eq!(
            fs::read(volume.join("data").join("record")).unwrap(),
            b"through the drive"
        );
    }
}
