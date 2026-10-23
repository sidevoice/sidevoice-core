//! Windows: a protected DACL set when a file or directory is created, read back from the handle; reparse points
//! (links, junctions, mount points) refused, at the path and on the way to it.
//!
//! "Private" is a protected DACL (it inherits nothing) granting full control to the owner (`OWNER RIGHTS`), SYSTEM
//! and Administrators only, inherited by what is created inside. Whether others may do anything is read from the
//! DACL of the handle that was opened (`GetSecurityInfo`): a DACL that is not protected counts (it takes whatever its
//! parent passes down), and so does an allow entry for anybody but the file's owner, this user, SYSTEM,
//! Administrators or `OWNER RIGHTS`, including one only what is created inside inherits; who the owner is, is
//! [`user`](crate::user)'s. The directories on the way are judged as the connector does (`trusted_ancestor`),
//! except that a reparse point among them is refused rather than followed.
//!
//! Own `unsafe` over `windows-sys`: no maintained crate covers "create with this DACL" and "read what this handle's
//! DACL grants" (`windows-acl` is unmaintained; `windows-permissions` is a thin binding with the same calls left to
//! us), and the surface is a few calls.

use super::Create;
use crate::user::{User, UserId};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;
use std::ptr::null_mut;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, GetSecurityDescriptorControl, ACL, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS, READ_CONTROL,
};

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
            Create::New => (GENERIC_WRITE, CREATE_NEW),
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

    fn open_to_others(file: &File) -> io::Result<bool> {
        let judged = Judged::of(file)?;
        Ok(!judged.protected()? || judged.grants_others(u32::MAX, true))
    }

    // A directory handle cannot be flushed (it needs write access, which a directory does not give), and NTFS
    // journals directory entries: nothing to do.
    fn sync_dir(_dir: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// A directory on the way to something private (the connector's rule): this user's or the system's, nobody but this
/// user and the system able to delete or rename what is in it, or to take it over. A reparse point on the way
/// (junction, symlink, mount point) is refused, not followed: only the path as written is judged, and one leading
/// into a directory others can change would hide it. A default profile has none on the way to its folders
/// (`C:\Users\<name>\AppData\Local\Temp`, `C:\Users\<name>\.sidevoice`): the legacy junctions (`Application Data`,
/// `Local Settings`, `C:\Documents and Settings`) sit beside that path, not on it. Adding entries is not judged: there
/// is no sticky bit, and `C:\` lets every user add folders.
pub(super) fn trusted_ancestor(dir: &Path) -> io::Result<bool> {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)?;
    if is_link(&file)? {
        return Ok(false);
    }
    let judged = Judged::of(&file)?;
    Ok(
        (UserId::current().owns_file(&file)? || judged.owner_is_system())
            && !judged.grants_others(REPLACES_ENTRIES, false),
    )
}

// Access rights (`winnt.h`), written out because they are the contract being checked.
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_DELETE_CHILD: u32 = 0x0040;
const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;
/// `FILE_GENERIC_WRITE` without `FILE_WRITE_DATA`: what appending needs and no more.
const APPEND: u32 = 0x0012_0116 & !FILE_WRITE_DATA;
/// What lets somebody replace what is in a directory, or take it over.
const REPLACES_ENTRIES: u32 = FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;

// ACE types and flags of a DACL (`winnt.h`).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 5;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 11;
/// The entry grants nothing to the object itself, only to what is created in it.
const INHERIT_ONLY_ACE: u8 = 0x08;

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
    /// `OWNER RIGHTS`.
    fn trusted(&self, sid: PSID) -> bool {
        let p = principals();
        // SAFETY: both point at valid SIDs (null is refused first).
        (!self.owner.is_null() && !sid.is_null() && unsafe { EqualSid(self.owner, sid) } != 0)
            || p.user.is(sid)
            || p.system.is(sid)
            || p.administrators.is(sid)
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

/// Lets everybody into `path` (tests).
#[cfg(test)]
pub(super) fn let_others_in(path: &Path) {
    icacls(path, &["/grant", "*S-1-1-0:(F)"]);
}

/// `icacls <path> <args>`, which must succeed; Everyone is `S-1-1-0` (tests).
#[cfg(test)]
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

/// Makes `link` lead to the file `target` (tests).
#[cfg(test)]
pub(super) fn link_file(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_file(target, link).unwrap();
}
