//! Windows: a protected DACL set when a file or directory is created (a record too: staged under that DACL, then
//! renamed over the record), read back from the handle; reparse points (links, junctions, mount points) refused at
//! the path itself. A symlink, junction or mount point on the way to a directory is followed only where nobody
//! untrusted can retarget it (`super::resolve_links`, [`check_link`]); what it leads to is then judged in full.
//!
//! "Private" is a protected DACL (it inherits nothing) granting full control to the owner (`OWNER RIGHTS`), SYSTEM
//! and Administrators only, inherited by what is created inside. What others may do is read from the DACL of the
//! handle that was opened (`GetSecurityInfo`): an allow entry for anybody but the file's owner, this user, SYSTEM,
//! Administrators, TrustedInstaller or `OWNER RIGHTS` counts; and, for whether it is private, so do a DACL that is not protected (it
//! takes whatever its parent passes down) and an entry for others that only what is created inside inherits. Who the
//! owner is, is [`user`](crate::user)'s.
//!
//! Own `unsafe` over `windows-sys`: no maintained crate covers "create with this DACL" and "read what this handle's
//! DACL grants" (`windows-acl` is unmaintained; `windows-permissions` is a thin binding with the same calls left to
//! us), and the surface is a few calls. The link count (and the identity of a file system with no 128-bit ids) comes
//! from `winapi-util`; a file's identity is `FileIdInfo`, one more call. A record's staging file (a fresh name beside
//! it, removed on failure) is `tempfile`'s; it is given the record's name by our own `MoveFileExW` with
//! `MOVEFILE_WRITE_THROUGH` (`tempfile` passes no such flag), replacing it or, for a record made once, replacing
//! nothing, and the directory is then flushed: through the held handle on it where there is one
//! ([`flush_held_dir`], `NtCreateFile` relative to that handle), by its path otherwise ([`flush_dir`]).

use super::{Access, Create, FileId, Fs, HeldState, Others, Publish};
use crate::user::{User, UserId};
use std::ffi::{c_void, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::OnceLock;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    NtCreateFile, FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_SUCCESS,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, ERROR_DIRECTORY, OBJ_CASE_INSENSITIVE, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, GetSecurityDescriptorControl, ACL, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FileIdInfo, GetDriveTypeW, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, MoveFileExW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_128, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MOVE_FILE_FLAGS,
    OPEN_ALWAYS, READ_CONTROL, VOLUME_NAME_GUID,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_READ, SYNCHRONIZE};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// This OS's files.
pub struct Os;

impl super::Fs for Os {
    fn create_private_dir(path: &Path) -> io::Result<()> {
        let attributes = PrivateAttributes::new()?;
        let wide = wide(path);
        // SAFETY: `wide` is NUL-terminated; the attributes are valid for the call.
        if unsafe { CreateDirectoryW(wide.as_ptr(), attributes.as_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    // `CreateFileW` because `std` cannot pass the security attributes of what it creates.
    fn open_private(path: &Path, how: Create) -> io::Result<File> {
        let (access, disposition) = match how {
            Create::New | Create::NewProgram => (GENERIC_WRITE, CREATE_NEW),
            Create::Append => (APPEND, OPEN_ALWAYS),
            Create::ReadWrite => (GENERIC_READ | GENERIC_WRITE, OPEN_ALWAYS),
        };
        let attributes = PrivateAttributes::new()?;
        let wide = wide(path);
        // SAFETY: `wide` is NUL-terminated and the attributes valid for the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                attributes.as_ptr(),
                disposition,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a handle just returned by `CreateFileW`, owned by nobody else.
        not_a_link(unsafe { File::from_raw_handle(handle as RawHandle) })
    }

    // `BACKUP_SEMANTICS` lets a directory be opened too.
    fn open_read(path: &Path) -> io::Result<File> {
        not_a_link(
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?,
        )
    }

    fn trusted_program(path: &Path) -> io::Result<PathBuf> {
        program_name(path)?;
        // Links on the way, the final one too, are followed only where trusted; the result has none left to hide.
        let path = &super::resolve_links(path)?;
        let name = program_name(path)?;
        let parent = Held::open_with(super::parent_of(path)?, None)?;
        let path = parent.0.leaf().0.join(name);
        let file = Self::open_read(&path).map_err(|error| match error.kind() {
            ErrorKind::NotFound => error,
            _ => super::refused_file(&path, error),
        })?;
        let judged = Judged::of(&file)?;
        if !file.metadata()?.is_file()
            || (!UserId::current().owns_file(&file)? && !judged.owner_is_system())
            || judged.grants_others(CHANGES, false)
        {
            return Err(super::denied(
                "unsafe program owner, type or write permissions",
            ));
        }
        let resolved = path.canonicalize()?;
        // An 8.3 alias can carry a different extension from its long name. Never return a script to launch.
        program_name(&resolved)?;
        // Verify that the returned spelling still names the opened file, with no final reparse traversal.
        let current = Self::open_read(&resolved)?;
        if Self::identity(&file)? != Self::identity(&current)? {
            return Err(super::denied("program replaced during validation"));
        }
        parent.0.verify()?;
        Ok(resolved)
    }

    fn sync_dir(dir: &Path) -> io::Result<()> {
        flush_dir(dir)
    }

    fn owner_of_user_directory_is_trusted(dir: &File) -> io::Result<bool> {
        Ok(UserId::current().owns_file(dir)? || Judged::of(dir)?.owner_is_system())
    }

    // `Change`: an entry for others that applies to the object itself. Anything else short of private is `Read`: an
    // entry for others of any kind, including one only what is created inside inherits (inherit-only too), and a DACL
    // that is not protected, which takes whatever its parent passes down, now or later.
    fn others_of(file: &File) -> io::Result<Others> {
        let judged = Judged::of(file)?;
        Ok(if judged.grants_others(CHANGES, false) {
            Others::Change
        } else if !judged.protected()? || judged.grants_others(u32::MAX, true) {
            Others::Read
        } else {
            Others::Nothing
        })
    }

    // The 128-bit file id (`FileIdInfo`): a 64-bit index is not unique on ReFS. A file system that has no such id
    // (the call is refused as unsupported) is told by `GetFileInformationByHandle`'s 64-bit index, through
    // `winapi-util`.
    fn identity(file: &File) -> io::Result<FileId> {
        let mut info = FILE_ID_INFO {
            VolumeSerialNumber: 0,
            FileId: FILE_ID_128 {
                Identifier: [0; 16],
            },
        };
        // SAFETY: the handle is open for the call; `info` is a `FILE_ID_INFO` of the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                FileIdInfo,
                &mut info as *mut FILE_ID_INFO as *mut c_void,
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if ok != 0 {
            return Ok(FileId {
                volume: info.VolumeSerialNumber,
                index: u128::from_le_bytes(info.FileId.Identifier),
            });
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
        ) {
            return Err(error);
        }
        let information = winapi_util::file::information(file)?;
        Ok(FileId {
            volume: information.volume_serial_number(),
            index: u128::from(information.file_index()),
        })
    }
}

/// How many names the file behind `file` has (`winapi-util`).
pub(super) fn link_count(file: &File) -> io::Result<u64> {
    Ok(winapi_util::file::information(file)?.number_of_links())
}

/// A directory on the way to something private: this user's or the system's, nobody but this user and the system able
/// to delete or rename what is in it, or to take it over. Entries only what is created inside inherits are not judged
/// here (`C:\` passes Modify down to Authenticated Users): what is ours is created with its own DACL. The root of a
/// local fixed volume (`D:\`, [`is_local_volume_root`]) cannot itself be deleted or renamed, so `DELETE` on it is not
/// counted: a second drive's root grants Authenticated Users Modify, which a directory below it that inherits it is
/// still refused for. Any other volume's root (a removable disk) is judged as any directory; a `subst` drive's root is
/// never one here, nor a share's ([`backing_directory`]: replaced by its real path, refused). A reparse point
/// here is refused: links are followed before the checks (`super::resolve_links`), so one that is still on the way is
/// not a link, or not one we follow (a redirected or cloud folder). Adding entries is not judged: there is no sticky
/// bit, and `C:\` lets every user add folders.
pub(super) fn check_ancestor(dir: &Path) -> io::Result<()> {
    check_ancestor_handle(dir, &open_entry(dir)?)
}

/// A symlink, junction or mount point on the way, in a directory that passed [`check_ancestor`]: followed only when
/// whoever may change it is trusted, as for a directory on the way (this user's or the system's, nobody else granted
/// any change to it), since whoever may write it may retarget it.
pub(super) fn check_link(link: &Path) -> io::Result<()> {
    let file = open_entry(link)?;
    let judged = Judged::of(&file)?;
    if (!UserId::current().owns_file(&file)? && !judged.owner_is_system())
        || judged.grants_others(CHANGES, false)
    {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("unsafe link {}: others can change it", link.display()),
        ));
    }
    Ok(())
}

/// The directory the root `root` (`X:\`, `\\server\share\`) stands for when it is not a volume's own root: a drive
/// letter `subst` gives a directory (`Z:\` for `C:\Shared\volume`), whose real path's ancestors (`C:\Shared`) can
/// rename or replace it however protected it is, so they are judged too, through that path. `None` for a local
/// volume's own root. The real path is the one the root's handle resolves to (`canonicalize`). A root on a network
/// (a share, a mapped drive) is refused: the directories behind it are on another machine, out of reach of any check
/// here.
pub(super) fn backing_directory(root: &Path) -> io::Result<Option<PathBuf>> {
    use std::path::{Component, Prefix};
    let backing = root.canonicalize()?;
    if !matches!(
        backing.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))
    ) {
        return Err(super::refused(
            root,
            "a network or shared drive; the directories behind it cannot be checked",
        ));
    }
    Ok(backing.parent().is_some().then_some(backing))
}

/// The entry `path` itself (a reparse point not followed), opened only to read its attributes and security.
fn open_entry(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

pub(super) fn check_ancestor_handle(dir: &Path, file: &File) -> io::Result<()> {
    let unsafe_dir = |why: &str| {
        io::Error::new(
            ErrorKind::PermissionDenied,
            format!("unsafe directory {}: {why}", dir.display()),
        )
    };
    if is_link(file)? || !file.metadata()?.is_dir() {
        return Err(unsafe_dir(
            "not an ordinary directory; choose an unredirected data directory",
        ));
    }
    let judged = Judged::of(file)?;
    if !UserId::current().owns_file(file)? && !judged.owner_is_system() {
        return Err(unsafe_dir("owned by another user"));
    }
    let replaces = if dir.parent().is_none() && is_local_volume_root(file) {
        REPLACES_ENTRIES & !DELETE
    } else {
        REPLACES_ENTRIES
    };
    if judged.grants_others(replaces, false) {
        return Err(unsafe_dir("others can replace what is in it"));
    }
    Ok(())
}

/// Writes `bytes` as `path` as `how` says ([`Publish`]): staged by `tempfile` (removed on failure, the rename's too)
/// in a file `open_private` creates private, then renamed over `path`, or, for `Publish::New`, renamed without
/// replacing (`MoveFileExW` with no `MOVEFILE_REPLACE_EXISTING`: the record never has a second name). For a private
/// directory, a handle on it (a reparse point refused) is judged (`super::judge_dir`) and held open, shared with no
/// `FILE_SHARE_DELETE`, until the record is in place: the directory judged cannot be renamed, removed or replaced
/// meanwhile, so the paths below name it. An existing file's DACL is not carried over to its replacement, which is
/// private like everything `open_private` creates.
///
/// Durability, as on Unix: once this returns `Ok`, the record survives a power cut whole, with the bytes written. The
/// staged file is flushed (`FlushFileBuffers`) before any name points at it; the rename is `MoveFileExW` with
/// `MOVEFILE_WRITE_THROUGH`, which returns only once the rename is on disk; the directory is then flushed
/// through the handle held on it ([`flush_held_dir`]), or by its path ([`flush_dir`]) only for a directory the
/// caller vouched for and holds nothing on. A power cut before that leaves the old record (or none, for
/// `Publish::New`), never a torn one.
pub(super) fn write_whole(path: &Path, bytes: &[u8], how: Publish) -> io::Result<()> {
    let path = &super::record_path(path, how)?;
    let dir = super::parent_of(path)?;
    let held = if how.judged() {
        let held = not_a_link(
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(dir)?,
        )?;
        super::judge_dir::<Os>(dir, &held)?;
        Some(held)
    } else {
        None
    };
    write_whole_in(path, bytes, how, held.as_ref(), || Ok(()))
}

/// The single staging/publication implementation; held-directory callers keep the entire checked ancestry alive.
/// `held` is a handle on the record's directory when the caller holds one: the directory is then flushed through it
/// ([`flush_held_dir`]); a directory the caller only vouched for, with no handle on it, is flushed by its path.
fn write_whole_in(
    path: &Path,
    bytes: &[u8],
    how: Publish,
    held: Option<&File>,
    before_publish: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // std supplies the extended-length spelling required by raw CreateFileW and tempfile's MoveFileExW, without
    // relying on a process manifest opting into long paths. Resolve only the already-checked (or vouched-for)
    // parent, never the record: a final link must not be followed. Held callers retain the original ancestry for
    // identity checks and keep it unrenameable throughout this conversion and publication.
    let dir = super::parent_of(path)?.canonicalize()?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "a record must have a file name"))?;
    let path = dir.join(name);
    let mut staged = tempfile::Builder::new()
        .prefix(&super::staging_prefix(&path))
        .rand_bytes(super::STAGING_RANDOM)
        .make_in(&dir, |temp| Os::open_private(temp, Create::New))?;
    staged.write_all(bytes)?;
    // `FlushFileBuffers`: the bytes are on disk before any name points at them.
    staged.as_file().sync_all()?;
    before_publish()?;
    // On failure `staged` is dropped with its staging name, which `tempfile` removes.
    move_write_through(staged.path(), &path, publish_flags(how))?;
    // The staging name is gone: nothing is left for `tempfile` to remove.
    staged.disable_cleanup(true);
    drop(staged);
    match held {
        Some(held) => flush_held_dir(held),
        None => flush_dir(&dir),
    }
}

/// The flags `write_whole_in` renames with: always `MOVEFILE_WRITE_THROUGH`, and `MOVEFILE_REPLACE_EXISTING` unless
/// the record is made once (`Publish::New`: it never replaces anything).
pub(super) fn publish_flags(how: Publish) -> MOVE_FILE_FLAGS {
    match how {
        Publish::New => MOVEFILE_WRITE_THROUGH,
        _ => MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
    }
}

/// Renames `from` to `to` (both in one directory, so on one volume), returning only once the rename is on disk.
/// `tempfile`'s `persist` is the same call without `MOVEFILE_WRITE_THROUGH`, which leaves the new name in the cache.
fn move_write_through(from: &Path, to: &Path, flags: MOVE_FILE_FLAGS) -> io::Result<()> {
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both are NUL-terminated and live for the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Flushes the directory `dir` (`FlushFileBuffers` on it), so what was just created, renamed or removed in it is on
/// disk. A directory is flushed only through a handle opened with `FILE_FLAG_BACKUP_SEMANTICS` and write access,
/// which `File::open` does not ask for: `FILE_ADD_FILE` (`FILE_WRITE_DATA`), the least write access the flush accepts
/// and one whoever put a file in `dir` has. Shared with everything, so a held handle on `dir` keeps its hold; a
/// reparse point there is refused, never flushed through. A file system that cannot flush a directory (it says the
/// call is not supported) has nothing more to give: the write-through rename is all there is.
fn flush_dir(dir: &Path) -> io::Result<()> {
    flush(&not_a_link(
        OpenOptions::new()
            .access_mode(FILE_WRITE_DATA)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(dir)?,
    )?)
}

/// As [`flush_dir`], for the directory a held handle is open on: reopened through that handle, never by path
/// ([`open_relative`] with an empty name, which `NtCreateFile` resolves to `RootDirectory` itself), so the directory
/// flushed is the one held, whatever its path names now.
fn flush_held_dir(held: &File) -> io::Result<()> {
    flush(&open_relative(held, OsStr::new(""), FILE_WRITE_DATA, true)?)
}

fn flush(dir: &File) -> io::Result<()> {
    match dir.sync_all() {
        Err(error) if flush_unsupported(&error) => Ok(()),
        flushed => flushed,
    }
}

/// Whether a flush failed only because the file system has no directory flush.
fn flush_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
    )
}

/// The held directory capability; public operations are declared in the shared interfaces.
pub struct Held(pub(crate) HeldState);

impl Held {
    fn open_with(path: &Path, access: Option<Access>) -> io::Result<Self> {
        // Links were followed where trusted (`super::resolve_links`): a reparse point left on the way is refused.
        let paths = super::real_directory_paths(path)?;
        let mut chain = Vec::new();
        // Root first: each checked path remains unrenameable before opening anything beneath it.
        for (index, path) in paths.iter().enumerate() {
            let leaf = index + 1 == paths.len();
            let file = reopen_directory(path)?;
            if let Some(access) = access.filter(|_| leaf) {
                super::judge_access(&file, access, true)
                    .map_err(|error| super::refused(path, error))?;
            } else {
                check_ancestor_handle(path, &file)?;
            }
            chain.push((path.clone(), file));
        }
        let held = Held(HeldState { chain, access });
        held.0.verify()?;
        Ok(held)
    }
}

impl super::Directory for Held {
    fn open(path: &Path, access: Access) -> io::Result<Self> {
        Self::open_with(path, Some(access))
    }

    fn open_beneath(&self, relative: &Path, access: Access) -> io::Result<Self> {
        self.0.beneath(relative, access).map(Held)
    }

    fn read(&self, name: &OsStr, access: Access, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let name = record_name(name)?;
        self.0.verify()?;
        let path = self.0.leaf().0.join(name);
        let Some(file) = open_record(&path, access)? else {
            return Ok(None);
        };
        let bytes = super::read_bounded(file, &path, limit)?;
        self.0.verify()?;
        Ok(Some(bytes))
    }

    fn write(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
        let name = record_name(name)?;
        self.0.verify()?;
        let path = self.0.leaf().0.join(name);
        open_record(&path, Access::NotWritableByOthers)?;
        write_whole_in(
            &path,
            bytes,
            Publish::Vouched,
            Some(self.0.leaf().1),
            || self.0.before_publish(),
        )
    }

    fn remove(&self, name: &OsStr) -> io::Result<()> {
        let name = record_name(name)?;
        self.0.verify()?;
        let path = self.0.leaf().0.join(name);
        if open_record(&path, Access::NotWritableByOthers)?.is_none() {
            return Ok(());
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// The directory named by `path`'s last component, opened relative to the handle `parent` ([`open_relative`]): no
/// directory above it is looked up by name again, so a reparse point set on a held directory (an empty one can
/// become a junction in place, which no share mode prevents) is never traversed. A reparse point at the child
/// itself is opened as itself and refused.
pub(super) fn open_subdirectory(
    parent: &File,
    path: &Path,
    _search_only: bool,
) -> io::Result<File> {
    let name = path
        .file_name()
        .ok_or_else(|| super::denied("invalid directory component"))?;
    let file = match open_relative(parent, name, FILE_GENERIC_READ, true) {
        Err(error) if error.raw_os_error() == Some(ERROR_DIRECTORY as i32) => {
            return Err(super::refused(path, "not an ordinary directory"));
        }
        opened => opened?,
    };
    ordinary_directory(path, file)
}

/// Opens the one child component `name` relative to the directory `parent` is open on: `NtCreateFile` with
/// `RootDirectory` set to that handle (Win32 has no handle-relative open), `FILE_OPEN_REPARSE_POINT` so a reparse
/// point there is opened itself and never followed, shared for reading and writing but never deletion, and
/// synchronous as std expects. `directory` requires a directory (`ERROR_DIRECTORY` otherwise). A missing child is
/// `NotFound`. `name` must already be validated as one ordinary component (`record_name`).
pub(crate) fn open_relative(
    parent: &File,
    name: &OsStr,
    access: u32,
    directory: bool,
) -> io::Result<File> {
    let wide: Vec<u16> = name.encode_wide().collect();
    let bytes = u16::try_from(wide.len() * 2)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "name too long"))?;
    let object_name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_ptr() as *mut u16,
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &object_name,
        // As Win32 opens are; NTFS keeps one entry per case-insensitive name.
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let options = FILE_OPEN_REPARSE_POINT
        | FILE_SYNCHRONOUS_IO_NONALERT
        | if directory { FILE_DIRECTORY_FILE } else { 0 };
    let mut handle: HANDLE = null_mut();
    // SAFETY: an all-zero IO_STATUS_BLOCK is valid (a status and a pointer-sized value), written by the call.
    let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: `parent` is open for the call; `attributes` points to `object_name`, whose buffer `wide` outlives the
    // call; `handle` and `status_block` are writable; no allocation size or extended attributes are passed.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: a pure status-code translation.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: the call succeeded, so `handle` is a new handle this process owns, handed to `File` to close.
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

/// Which volume a handle is on: Windows has no mount below a volume other than a reparse point (a mount point),
/// which descent and removal refuse as one, so a volume serial is the whole mount identity.
#[derive(PartialEq, Debug)]
pub(crate) struct Mount(u64);

pub(crate) fn mount_of(file: &File) -> io::Result<Mount> {
    Ok(Mount(<Os as Fs>::identity(file)?.volume))
}

pub(super) fn reopen_directory(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        // A metadata-only access mask does not participate in Windows sharing checks.
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    ordinary_directory(path, file)
}

/// `file`, refused unless it is a directory and no reparse point (opened with `OPEN_REPARSE_POINT`).
fn ordinary_directory(path: &Path, file: File) -> io::Result<File> {
    if is_link(&file)? || !file.metadata()?.is_dir() {
        return Err(super::refused(
            path,
            "not an ordinary directory; choose an unredirected data directory",
        ));
    }
    Ok(file)
}

fn open_record(path: &Path, access: Access) -> io::Result<Option<File>> {
    let file = match Os::open_read(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(super::refused_file(path, error)),
    };
    super::judge_access(&file, access, false)?;
    Ok(Some(file))
}

fn program_name(path: &Path) -> io::Result<&Path> {
    let name = record_name(super::program_name(path)?)?;
    if !name
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Err(super::denied("expected a native .exe program"));
    }
    Ok(name)
}

/// Win32 aliases must not turn an ordinary child into a device, stream or differently spelled entry.
pub(crate) fn record_name(name: &OsStr) -> io::Result<&Path> {
    let path = super::child_name(name)?;
    let text = name.to_string_lossy();
    let stem = text
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_uppercase();
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if device
        || text.ends_with(['.', ' '])
        || text.chars().any(|c| c < ' ' || "<>:\"|?*".contains(c))
    {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "expected an ordinary Windows child name",
        ));
    }
    Ok(path)
}

/// Nothing to finish (`Fs::open_created_file`): `write_whole` gives a record made once its name by a rename that
/// replaces nothing, so it never has a second name.
pub(super) fn finish_creation(_path: &Path) -> io::Result<()> {
    Ok(())
}

// Access rights (`winnt.h`), written out because they are the contract being checked.
const FILE_WRITE_DATA: u32 = 0x0002; // FILE_ADD_FILE on a directory
const FILE_APPEND_DATA: u32 = 0x0004; // FILE_ADD_SUBDIRECTORY on a directory
const FILE_WRITE_EA: u32 = 0x0010;
const FILE_DELETE_CHILD: u32 = 0x0040;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;
/// `FILE_GENERIC_WRITE` without `FILE_WRITE_DATA`: what appending needs and no more.
const APPEND: u32 = 0x0012_0116 & !FILE_WRITE_DATA;

/// What lets somebody change a file or what is in a directory, or who may.
const CHANGES: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;
/// What lets somebody replace what is in a directory, or take it over.
const REPLACES_ENTRIES: u32 = FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;

// ACE types and flags of a DACL (`winnt.h`).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
/// The entry grants nothing to the object itself, only to what is created in it.
const INHERIT_ONLY_ACE: u8 = 0x08;

/// Whether the directory held by `dir` is the root of a local fixed volume, judged from the handle, not the path's
/// spelling: its final path by volume GUID is that volume's root (`\\?\Volume{…}\`), and the volume is a fixed disk.
/// A drive letter given to a directory (`subst`) names a directory on another volume, a mapped drive or a share has no
/// local volume, and a removable one may be someone else's: none of them is a root nobody can rename.
pub(super) fn is_local_volume_root(dir: &File) -> bool {
    let mut path = [0u16; 128];
    // SAFETY: the handle is open for the call; the buffer's length is passed with it.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            dir.as_raw_handle() as HANDLE,
            path.as_mut_ptr(),
            path.len() as u32,
            VOLUME_NAME_GUID,
        )
    } as usize;
    // 0 is a failure (a remote file has no volume GUID); a length beyond the buffer is no volume root's.
    if length == 0 || length >= path.len() {
        return false;
    }
    let name = String::from_utf16_lossy(&path[..length]);
    let root = name
        .strip_prefix(r"\\?\Volume{")
        .and_then(|rest| rest.strip_suffix(r"}\"))
        .is_some_and(|guid| !guid.contains('\\'));
    // SAFETY: a NUL-terminated wide string that outlives the call.
    root && unsafe { GetDriveTypeW(wide(Path::new(&name)).as_ptr()) } == DRIVE_FIXED
}

/// The private descriptor: protected, full control to the owner, SYSTEM and Administrators, inherited by what is
/// created inside. The owner is left to the system: the token's default owner, this user.
const PRIVATE_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Refuses a handle that turned out to be a link of any kind (the open did not follow it).
fn not_a_link(file: File) -> io::Result<File> {
    if is_link(&file)? {
        return Err(io::Error::other("a link (reparse point)"));
    }
    Ok(file)
}

/// Whether a handle opened with `OPEN_REPARSE_POINT` is a reparse point of any kind: a symlink, a junction, a mount
/// point.
fn is_link(file: &File) -> io::Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.is_symlink() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// An owned SID, parsed from its string form.
struct Sid(PSID);

// SAFETY: the SID is immutable once made and freed only on drop.
unsafe impl Send for Sid {}
unsafe impl Sync for Sid {}

impl Sid {
    fn parse(text: &str) -> Sid {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sid: PSID = null_mut();
        // SAFETY: `wide` is NUL-terminated; on success `sid` is allocated with `LocalAlloc`.
        let ok = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
        assert!(ok != 0, "a well-known SID parses");
        Sid(sid)
    }

    fn is(&self, other: PSID) -> bool {
        // SAFETY: both point at valid SIDs (null is refused first).
        !other.is_null() && unsafe { EqualSid(self.0, other) } != 0
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: allocated by `ConvertStringSidToSidW` with `LocalAlloc`.
        unsafe { LocalFree(self.0) };
    }
}

/// This user, SYSTEM, Administrators and `OWNER RIGHTS`: who may hold anything in what is ours besides its owner; and
/// TrustedInstaller, which owns `C:\` and the system's folders.
struct Principals {
    user: UserId,
    system: Sid,
    administrators: Sid,
    owner_rights: Sid,
    trusted_installer: Sid,
}

fn principals() -> &'static Principals {
    static PRINCIPALS: OnceLock<Principals> = OnceLock::new();
    PRINCIPALS.get_or_init(|| Principals {
        user: UserId::current(),
        system: Sid::parse("SY"),
        administrators: Sid::parse("BA"),
        owner_rights: Sid::parse("OW"),
        trusted_installer: Sid::parse(
            "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
        ),
    })
}

/// The owner and DACL of an open handle, released on drop.
struct Judged {
    owner: PSID,
    dacl: *mut ACL,
    descriptor: *mut c_void,
}

impl Drop for Judged {
    fn drop(&mut self) {
        // SAFETY: allocated by `GetSecurityInfo` with `LocalAlloc`; `owner` and `dacl` point inside it.
        unsafe { LocalFree(self.descriptor) };
    }
}

impl Judged {
    fn of(file: &File) -> io::Result<Judged> {
        let mut judged = Judged {
            owner: null_mut(),
            dacl: null_mut(),
            descriptor: null_mut(),
        };
        // SAFETY: the handle is open for the call; on success the pointers are into `descriptor`.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut judged.owner,
                null_mut(),
                &mut judged.dacl,
                null_mut(),
                &mut judged.descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(judged)
    }

    /// Whether the DACL is protected: it takes nothing its parent passes down.
    fn protected(&self) -> io::Result<bool> {
        let (mut control, mut revision) = (0u16, 0u32);
        // SAFETY: `descriptor` is the valid descriptor `GetSecurityInfo` returned, alive as long as `self`.
        if unsafe { GetSecurityDescriptorControl(self.descriptor, &mut control, &mut revision) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(control & SE_DACL_PROTECTED != 0)
    }

    /// Whether the owner is the system: SYSTEM, Administrators or TrustedInstaller.
    fn owner_is_system(&self) -> bool {
        let p = principals();
        p.system.is(self.owner)
            || p.administrators.is(self.owner)
            || p.trusted_installer.is(self.owner)
    }

    /// Who may hold anything in what is ours: its owner; this user, whoever the owner is (a profile folder owned by
    /// Administrators or SYSTEM that grants its user full control is a default layout); SYSTEM, Administrators and
    /// TrustedInstaller (also on a user-owned directory inheriting Program Files access), and `OWNER RIGHTS`.
    fn trusted(&self, sid: PSID) -> bool {
        let p = principals();
        // SAFETY: both point at valid SIDs (null is refused first).
        (!self.owner.is_null() && !sid.is_null() && unsafe { EqualSid(self.owner, sid) } != 0)
            || p.user.is(sid)
            || p.system.is(sid)
            || p.administrators.is(sid)
            || p.trusted_installer.is(sid)
            || p.owner_rights.is(sid)
    }

    /// Whether somebody untrusted is granted anything in `mask`, counting the entries only what is created inside
    /// inherits when `inherit_only` says so. No DACL at all grants everything to everybody; an allow entry of a layout
    /// not read here is taken to grant it.
    fn grants_others(&self, mask: u32, inherit_only: bool) -> bool {
        if self.dacl.is_null() {
            return true;
        }
        // SAFETY: `dacl` points at a valid ACL inside the descriptor, which lives as long as `self`.
        let count = unsafe { (*self.dacl).AceCount };
        for index in 0..u32::from(count) {
            let mut ace: *mut c_void = null_mut();
            // SAFETY: `index` is below the ACL's entry count.
            if unsafe { GetAce(self.dacl, index, &mut ace) } == 0 || ace.is_null() {
                return true;
            }
            // SAFETY: every ACE starts with its header (type, flags, size), then the access mask; for the plain
            // allow entry the SID follows.
            let (kind, flags, access) = unsafe {
                let header = *(ace as *const [u8; 4]);
                (
                    header[0],
                    header[1],
                    *((ace as *const u8).add(4) as *const u32),
                )
            };
            if (flags & INHERIT_ONLY_ACE != 0 && !inherit_only) || access & mask == 0 {
                continue;
            }
            match kind {
                ACCESS_ALLOWED_ACE_TYPE => {
                    // SAFETY: as above.
                    let sid = unsafe { (ace as *const u8).add(8) as PSID };
                    if !self.trusted(sid) {
                        return true;
                    }
                }
                ACCESS_ALLOWED_OBJECT_ACE_TYPE
                | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
                | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => return true,
                // Deny entries and the rest grant nothing.
                _ => {}
            }
        }
        false
    }
}

/// The security attributes of a new private file or directory, and the descriptor they point at.
struct PrivateAttributes(SECURITY_ATTRIBUTES);

impl PrivateAttributes {
    fn new() -> io::Result<Self> {
        let sddl: Vec<u16> = PRIVATE_SDDL
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor: *mut c_void = null_mut();
        // SAFETY: the SDDL is NUL-terminated; the descriptor is allocated by the call and released on drop.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        }))
    }

    fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.0
    }
}

impl Drop for PrivateAttributes {
    fn drop(&mut self) {
        // SAFETY: allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW` with `LocalAlloc`.
        unsafe { LocalFree(self.0.lpSecurityDescriptor) };
    }
}

#[cfg(any(test, feature = "test-support"))]
impl super::TestFiles for Os {
    fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) {
        let (target, link) = (target.as_ref(), link.as_ref());
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link).unwrap();
        } else {
            std::os::windows::fs::symlink_file(target, link).unwrap();
        }
    }

    // The extension decides what runs: nothing to mark.
    fn make_executable(_: &Path) {}

    fn let_others_read(path: &Path) {
        icacls(path, &["/grant", "*S-1-1-0:(RX)"]);
    }

    fn let_others_change(path: &Path) {
        icacls(path, &["/grant", "*S-1-1-0:(F)"]);
    }

    // Inheritance off first (what was inherited is kept as the file's own entries and the DACL protected), so a file a
    // test wrote with `std` in a scratch directory ends up as private as one `open_private` makes.
    fn shut_others_out(path: &Path) {
        icacls(path, &["/inheritance:d"]);
        icacls(path, &["/remove:g", "*S-1-1-0"]);
    }
}

/// Turns the existing empty directory `dir` into a junction to `target` in place (`FSCTL_SET_REPARSE_POINT`, as
/// any process of this user can, even while `dir` is held without delete sharing). Tests only; must succeed.
#[cfg(test)]
pub(crate) fn set_junction(dir: &Path, target: &Path) {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    let target = target.to_string_lossy();
    let target = target.strip_prefix(r"\\?\").unwrap_or(&target);
    // The mount-point reparse buffer: tag, data length, reserved; substitute name (offset, length) and an empty print
    // name (offset, length); then both names, each NUL-terminated.
    let substitute: Vec<u16> = format!(r"\??\{target}").encode_utf16().collect();
    let substitute_bytes = (substitute.len() * 2) as u16;
    let mut names = substitute;
    names.extend([0, 0]);
    let mut data = Vec::new();
    data.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    data.extend((8 + names.len() as u16 * 2).to_le_bytes());
    data.extend(0u16.to_le_bytes());
    data.extend(0u16.to_le_bytes());
    data.extend(substitute_bytes.to_le_bytes());
    data.extend((substitute_bytes + 2).to_le_bytes());
    data.extend(0u16.to_le_bytes());
    for unit in names {
        data.extend(unit.to_le_bytes());
    }
    let file = OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)
        .unwrap();
    let mut returned = 0u32;
    // SAFETY: the handle is open for the call; `data` is a complete reparse buffer of the length passed; no output.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_SET_REPARSE_POINT,
            data.as_ptr() as *const c_void,
            data.len() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    assert!(ok != 0, "{}: {}", dir.display(), io::Error::last_os_error());
}

/// `icacls <path> <args>` (tests), which must succeed. Everyone is `S-1-1-0`.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn icacls(path: &Path, args: &[&str]) {
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "icacls {args:?}");
}

/// Makes `link` lead to the directory `target`: a junction, which needs no privilege (tests).
#[cfg(test)]
pub(super) fn link_dir(target: &Path, link: &Path) {
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "mklink /J");
}

#[cfg(test)]
pub(super) fn system_program() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").expect("Windows directory"))
        .join("System32")
        .join("whoami.exe")
}

// Native Windows fixtures: no assertions about trust policy live here.
#[cfg(test)]
pub(super) fn ordinary_path(path: &Path) -> PathBuf {
    let text = path.to_str().unwrap();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
}

#[cfg(test)]
pub(super) fn short_name(path: &Path, alias: &str) -> PathBuf {
    let path = ordinary_path(path);
    let output = std::process::Command::new("fsutil")
        .args(["file", "setshortname"])
        .arg(&path)
        .arg(alias)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "setshortname: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    path.parent().unwrap().join(alias)
}

#[cfg(test)]
pub(super) struct Share(String);

#[cfg(test)]
impl Share {
    pub(super) fn new(path: &Path) -> Self {
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Service LanmanServer"])
            .status()
            .unwrap();
        assert!(status.success(), "start local SMB server for UNC fixture");
        let share = Self(format!("sidevoice-p2-{}", std::process::id()));
        let output = std::process::Command::new("net")
            .arg("share")
            .arg(format!("{}={}", share.0, ordinary_path(path).display()))
            .arg(format!("/GRANT:{},FULL", UserId::name().unwrap()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "net share: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        share
    }

    pub(super) fn path(&self) -> PathBuf {
        PathBuf::from(format!(r"\\localhost\{}", self.0))
    }
}

#[cfg(test)]
impl Drop for Share {
    fn drop(&mut self) {
        let _ = std::process::Command::new("net")
            .args(["share", &self.0, "/DELETE", "/Y"])
            .output();
    }
}

/// A free drive letter given to a directory with `subst` (tests), removed on drop: the directory is that drive's root.
#[cfg(test)]
pub(super) struct Drive(String);

#[cfg(test)]
impl Drive {
    pub(super) fn new(path: &Path) -> Self {
        let letter = ('G'..='Z')
            .rev()
            .map(|letter| format!("{letter}:"))
            .find(|drive| !Path::new(&format!(r"{drive}\")).exists())
            .expect("a free drive letter");
        let output = std::process::Command::new("subst")
            .arg(&letter)
            .arg(ordinary_path(path))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "subst: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        Self(letter)
    }

    pub(super) fn root(&self) -> PathBuf {
        PathBuf::from(format!(r"{}\", self.0))
    }
}

#[cfg(test)]
impl Drop for Drive {
    fn drop(&mut self) {
        let _ = std::process::Command::new("subst")
            .args([&self.0, "/D"])
            .output();
    }
}

#[cfg(test)]
pub(super) struct OtherUser(pub(super) String);

#[cfg(test)]
impl OtherUser {
    pub(super) fn new() -> Self {
        let name = format!("svp2{}", std::process::id());
        // Fourteen characters avoids net.exe's interactive legacy-password-length prompt.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let password = format!("Sv!{:011x}", nonce & 0xffffffffff);
        let output = std::process::Command::new("net")
            .args(["user", &name, &password, "/ADD"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "create test principal: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(name)
    }
}

#[cfg(test)]
impl Drop for OtherUser {
    fn drop(&mut self) {
        let _ = std::process::Command::new("net")
            .args(["user", &self.0, "/DELETE"])
            .output();
    }
}
