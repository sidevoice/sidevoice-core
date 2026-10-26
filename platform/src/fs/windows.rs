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
//! it, removed on failure, renamed over it or, for a record made once, renamed replacing nothing) is `tempfile`'s.

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

use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_SUCCESS,
    HANDLE, INVALID_HANDLE_VALUE,
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
    GetFinalPathNameByHandleW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_128, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
    READ_CONTROL, VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

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

    // NTFS journals its metadata, and a directory can be flushed only through a handle opened for writing with
    // `FILE_FLAG_BACKUP_SEMANTICS`, which `File::open` does not ask for (it fails): nothing to do.
    fn sync_dir(_: &Path) -> io::Result<()> {
        Ok(())
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
/// still refused for. Any other root (a `subst` drive, a mapped drive, a share) is judged as any directory. A reparse point
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
/// private like everything `open_private` creates. The directory is not synced (`Fs::sync_dir`).
pub(super) fn write_whole(path: &Path, bytes: &[u8], how: Publish) -> io::Result<()> {
    let path = &super::record_path(path, how)?;
    let dir = super::parent_of(path)?;
    let _held = if how.judged() {
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
    write_whole_in(path, bytes, how, || Ok(()))
}

/// The single staging/publication implementation; held-directory callers keep the entire checked ancestry alive.
fn write_whole_in(
    path: &Path,
    bytes: &[u8],
    how: Publish,
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
    staged.as_file().sync_all()?;
    before_publish()?;
    match how {
        Publish::New => staged.persist_noclobber(&path),
        _ => staged.persist(&path),
    }
    .map(drop)
    .map_err(|error| error.error)
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
        write_whole_in(&path, bytes, Publish::Vouched, || self.0.before_publish())
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

pub(super) fn reopen_directory(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        // A metadata-only access mask does not participate in Windows sharing checks.
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
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
