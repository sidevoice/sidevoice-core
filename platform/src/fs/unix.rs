//! Unix: owner and mode bits, `O_NOFOLLOW`, the descriptor judged as opened, a record written relative to a handle on
//! its directory (`rustix`); who the administrators are, from the system's user and group databases (`nix`); an extended
//! ACL (Linux: whether there is one, `rustix`; macOS: its entries, `exacl`, and an open file's by its descriptor,
//! [`acl_by_fd`], the only `unsafe` here).
//!
//! Who is trusted besides this user: root and the administrators, the members of [`ADMIN_GROUPS`]. They may own what
//! is on the way to our files, and one of those groups, or this user's own private group, may write in it. Anybody else
//! able to write there (another group, everybody, anybody an extended ACL lets write: [`acl_others`]) makes it unsafe,
//! unless the sticky bit keeps them to their own entries (an ACL's writers are refused even then).

use super::{Access, Create, FileId, HeldState, Others, Publish};
use crate::user::{User, UserId};
use nix::unistd::{Gid, Group, Uid, User as Account};
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{self as rfs, AtFlags, Mode, OFlags};
use rustix::io::Errno;

/// This OS's files.
pub struct Os;

impl super::Fs for Os {
    fn create_private_dir(path: &Path) -> io::Result<()> {
        DirBuilder::new().mode(0o700).create(path)
    }

    fn open_private(path: &Path, how: Create) -> io::Result<File> {
        let mut options = OpenOptions::new();
        match how {
            Create::New => options.write(true).create_new(true).mode(0o600),
            Create::NewProgram => options.write(true).create_new(true).mode(0o700),
            Create::Append => options.append(true).create(true).mode(0o600),
            Create::ReadWrite => options
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600),
        };
        options.custom_flags(flags(OFlags::NOFOLLOW)).open(path)
    }

    fn open_read(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(flags(OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY))
            .open(path)
    }

    fn trusted_program(path: &Path) -> io::Result<PathBuf> {
        super::program_name(path)?;
        // Preserve the desktop's resolved-program policy: the caller never launches the original link again.
        let resolved = super::resolve_links(path).map_err(|error| directory_error(path, error))?;
        let parent = Held::open_with(super::parent_of(&resolved)?, None)?;
        let name = super::child_name(super::program_name(&resolved)?)?;
        // fstatat requires search access to the held directory, not read access to the executable. It also
        // avoids opening devices/FIFOs, and refuses a final link substituted after resolution.
        let metadata = rfs::statat(parent.0.leaf().1, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if rfs::FileType::from_raw_mode(metadata.st_mode) != rfs::FileType::RegularFile
            || metadata.st_mode & 0o1000 != 0
            || metadata.st_mode & 0o111 == 0
            || untrusted(
                metadata.st_uid,
                metadata.st_gid,
                metadata.st_mode,
                Object::Entry(&resolved),
            )?
            .is_some()
        {
            return Err(super::denied(
                "unsafe program owner, type or executable permissions",
            ));
        }
        parent.0.verify()?;
        Ok(resolved)
    }

    fn sync_dir(dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    fn owner_of_user_directory_is_trusted(dir: &File) -> io::Result<bool> {
        trusted_owner(dir.metadata()?.uid())
    }

    // A trusted group's write bit is `Read`: nobody untrusted may change it, but it is not private either.
    fn others_of(file: &File) -> io::Result<Others> {
        let metadata = file.metadata()?;
        let mode = metadata.mode();
        let acl = acl_others(Object::Open(file), mode)?;
        Ok(
            if acl == Others::Change || others_can_write(metadata.gid(), mode)? {
                Others::Change
            } else if mode & 0o077 != 0 || acl == Others::Read {
                Others::Read
            } else {
                Others::Nothing
            },
        )
    }

    fn identity(file: &File) -> io::Result<FileId> {
        let metadata = file.metadata()?;
        Ok(FileId {
            volume: metadata.dev(),
            index: metadata.ino().into(),
        })
    }
}

/// How many names the file behind `file` has.
pub(super) fn link_count(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.nlink())
}

/// The administrators' groups, by name, looked up in the system's group database (a name missing on this machine is
/// skipped). Their members are trusted as much as root: an account in one of them (as its primary group or listed as
/// a member) may own a directory on the way, and the group itself may write in one.
/// - macOS: `admin` (the accounts marked as administrators) and `wheel` (gid 0, root's group).
/// - Linux and other Unix: `sudo` (Debian, Ubuntu), `wheel` (Fedora, Arch, SUSE) and `admin` (older Ubuntu).
#[cfg(target_os = "macos")]
const ADMIN_GROUPS: &[&str] = &["admin", "wheel"];
#[cfg(not(target_os = "macos"))]
const ADMIN_GROUPS: &[&str] = &["sudo", "wheel", "admin"];

/// Whether the account `uid` may own what is on the way to our files: root, this user, or an administrator (an account
/// whose primary group is one of [`ADMIN_GROUPS`] or that is listed in one). An unknown account is not trusted.
fn trusted_owner(uid: u32) -> io::Result<bool> {
    if uid == 0 || uid == rustix::process::geteuid().as_raw() {
        return Ok(true);
    }
    let Some(account) = Account::from_uid(Uid::from_raw(uid))? else {
        return Ok(false);
    };
    let mut admin_groups = Vec::new();
    for name in ADMIN_GROUPS {
        admin_groups.extend(Group::from_name(name)?);
    }
    Ok(is_administrator(&account.name, account.gid, &admin_groups))
}

/// Whether the account `name`, whose primary group is `gid`, is an administrator: one of `admin_groups` (those of
/// [`ADMIN_GROUPS`] this machine has) is its primary group or lists it as a member.
fn is_administrator(name: &str, gid: Gid, admin_groups: &[Group]) -> bool {
    admin_groups
        .iter()
        .any(|group| group.gid == gid || group.mem.iter().any(|member| member == name))
}

/// Whether the group `gid` may write in what is on the way to our files: one of [`ADMIN_GROUPS`], or this user's own
/// private group ([`is_private_group`]). A group or account that cannot be looked up is neither: not trusted.
fn trusted_group(gid: u32) -> bool {
    let Ok(Some(group)) = Group::from_gid(Gid::from_raw(gid)) else {
        return false;
    };
    if ADMIN_GROUPS.contains(&group.name.as_str()) {
        return true;
    }
    Account::from_uid(Uid::effective())
        .ok()
        .flatten()
        .is_some_and(|me| is_private_group(&group, &me))
}

/// Whether `group` is the private group of the account `me` (Linux's user private groups, with umask `002`): its
/// primary group, named as it, and listing nobody else as a member.
///
/// Nobody else's membership is proved: no account is enumerated and the sources of the databases (NSS: files, SSSD,
/// LDAP...) are not inspected, so another account with this primary group, or a second group record with this gid,
/// would not show. Who belongs to which group is configured only by the administrators (the user and group
/// databases, whatever serves them), and the administrators are trusted ([`ADMIN_GROUPS`]); a user who is not one
/// cannot make themselves a member of another user's private group.
fn is_private_group(group: &Group, me: &Account) -> bool {
    group.gid == me.gid
        && group.name == me.name
        && group.mem.iter().all(|member| *member == me.name)
}

/// Whether somebody untrusted may write in what has group `gid` and permission bits `mode`: everybody (`o+w`), or a
/// group that is not trusted ([`trusted_group`]). An extended ACL is [`acl_others`]'. The sticky bit is the caller's
/// to weigh.
fn others_can_write(gid: u32, mode: u32) -> io::Result<bool> {
    Ok(mode & 0o002 != 0 || (mode & 0o020 != 0 && !trusted_group(gid)))
}

/// Where a judged object's extended ACL is read: an open handle (also one opened only to search, `O_PATH`), or an
/// entry named by a path that is not followed if it is a link.
#[derive(Clone, Copy)]
enum Object<'a> {
    Open(&'a File),
    Entry(&'a Path),
}

/// What the extended ACL of `object`, whose mode is `mode`, lets anybody else do beyond the mode bits: `Change` when
/// somebody untrusted may write it, `Read` when it grants anybody else anything, `Nothing` without one.
///
/// Linux (POSIX.1e): with an extended ACL the group bits are its mask, the most any named user or group (or the owning
/// group) gets; with no write there nobody named may write. With write there, who they are is not weighed: an extended
/// access ACL (`system.posix_acl_access`, present only when it says more than the mode) is somebody untrusted writing,
/// as the group bit was before trusted groups. Reading is already in the mode bits.
#[cfg(target_os = "linux")]
fn acl_others(object: Object<'_>, mode: u32) -> io::Result<Others> {
    use std::os::fd::AsRawFd;
    const ACCESS_ACL: &str = "system.posix_acl_access";
    if mode & 0o020 == 0 {
        return Ok(Others::Nothing);
    }
    let size: &mut [u8] = &mut [];
    let found = match object {
        // A handle opened only to search (`O_PATH`) has no `fgetxattr`: its `/proc` name leads to the same inode.
        Object::Open(file) => rfs::getxattr(
            format!("/proc/self/fd/{}", file.as_raw_fd()),
            ACCESS_ACL,
            size,
        ),
        Object::Entry(path) => rfs::lgetxattr(path, ACCESS_ACL, size),
    };
    match found {
        Ok(_) => Ok(Others::Change),
        Err(Errno::NODATA | Errno::OPNOTSUPP) => Ok(Others::Nothing),
        Err(error) => Err(error.into()),
    }
}

/// macOS: an ACL's entries come on top of the mode bits, which do not show them. An entry that allows (deny entries
/// only take away) and applies to the object itself (not only to what is created inside) lets its user or group write
/// when it grants any change (data, entries, attributes, security, owner); that is somebody untrusted writing unless
/// the user may own what is on the way ([`trusted_owner`]) or the group may write there ([`trusted_group`]). One that
/// names nobody this machine knows is untrusted. An allow entry for anybody but this user is somebody else reading,
/// one only what is created inside inherits too: what is made in it would not be private ([`check_inherited`]).
#[cfg(target_os = "macos")]
fn acl_others(object: Object<'_>, _mode: u32) -> io::Result<Others> {
    use exacl::{Flag, Perm};
    let changes = Perm::WRITE
        | Perm::APPEND
        | Perm::DELETE
        | Perm::DELETE_CHILD
        | Perm::WRITEATTR
        | Perm::WRITEEXTATTR
        | Perm::WRITESECURITY
        | Perm::CHOWN;
    let mut others = Others::Nothing;
    for Granted {
        perms,
        flags,
        trusted,
        mine,
    } in granted(object)?
    {
        if perms.intersects(changes) && !trusted && !flags.contains(Flag::ONLY_INHERIT) {
            return Ok(Others::Change);
        }
        if !mine {
            others = Others::Read;
        }
    }
    Ok(others)
}

/// Who an ACL entry names (macOS): a user or a group by id, or anybody this machine does not know (a GUID it cannot
/// map).
#[cfg(target_os = "macos")]
enum Who {
    User(u32),
    Group(u32),
    Unknown,
}

/// An allow entry of an ACL (macOS): who it names, what it grants and how it is inherited.
#[cfg(target_os = "macos")]
struct Allowed {
    who: Who,
    perms: exacl::Perm,
    flags: exacl::Flag,
}

/// An allow entry of an ACL (macOS), with whether who it names is trusted ([`trusted_owner`], [`trusted_group`]) and
/// whether it is this user.
#[cfg(target_os = "macos")]
struct Granted {
    perms: exacl::Perm,
    flags: exacl::Flag,
    trusted: bool,
    mine: bool,
}

/// The allow entries of `object`'s ACL (macOS), [`Granted`]; deny entries only take away. An open file's ACL is read
/// by its descriptor ([`acl_by_fd::allowed`]), never by a name of it; an entry's by its path, not following a link.
#[cfg(target_os = "macos")]
fn granted(object: Object<'_>) -> io::Result<Vec<Granted>> {
    use exacl::{AclEntryKind, AclOption};
    let allowed = match object {
        Object::Open(file) => acl_by_fd::allowed(file)?,
        Object::Entry(path) => {
            let mut allowed = Vec::new();
            for entry in exacl::getfacl(path, AclOption::SYMLINK_ACL)? {
                if !entry.allow {
                    continue;
                }
                let who = match entry.kind {
                    AclEntryKind::User => account_id(&entry.name)?.map_or(Who::Unknown, Who::User),
                    AclEntryKind::Group => group_id(&entry.name)?.map_or(Who::Unknown, Who::Group),
                    _ => Who::Unknown,
                };
                allowed.push(Allowed {
                    who,
                    perms: entry.perms,
                    flags: entry.flags,
                });
            }
            allowed
        }
    };
    let me = Uid::effective();
    let mut granted = Vec::new();
    for Allowed { who, perms, flags } in allowed {
        let (trusted, mine) = match who {
            Who::User(uid) => (trusted_owner(uid)?, uid == me.as_raw()),
            Who::Group(gid) => (trusted_group(gid), false),
            Who::Unknown => (false, false),
        };
        granted.push(Granted {
            perms,
            flags,
            trusted,
            mine,
        });
    }
    Ok(granted)
}

/// An open file's ACL read by its descriptor (macOS, `acl_get_fd_np`), the same libSystem calls `exacl` makes for a
/// path, which it offers no descriptor form of. No name of the file is looked up, so none moved or removed meanwhile
/// (an ancestor renamed, the other name of a file with two removed as a creation is finished) changes what is read,
/// and no stale name (`F_GETPATH` keeps the one a file was created by after it is removed) can stand in for it.
///
/// The only `unsafe` of this module: calls into libSystem on a descriptor `file` keeps open for the call, and on an ACL
/// it returns, freed once ([`Freed`]) after every entry, permission and flag set read from it (all owned by the ACL).
#[cfg(target_os = "macos")]
mod acl_by_fd {
    use super::{answer, Allowed, Who};
    use exacl::{Flag, Perm};
    use rustix::io::Errno;
    use std::ffi::{c_int, c_uint, c_void};
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::ptr;

    // <sys/acl.h>, <membership.h>.
    const ACL_TYPE_EXTENDED: c_uint = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_uint = 1;
    const ID_TYPE_UID: c_int = 0;
    const ID_TYPE_GID: c_int = 1;

    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, kind: c_uint) -> *mut c_void;
        fn acl_valid(acl: *mut c_void) -> c_int;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_uint) -> c_int;
        fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
        fn acl_get_permset(entry: *mut c_void, permset: *mut *mut c_void) -> c_int;
        fn acl_get_perm_np(permset: *mut c_void, perm: c_uint) -> c_int;
        fn acl_get_flagset_np(object: *mut c_void, flagset: *mut *mut c_void) -> c_int;
        fn acl_get_flag_np(flagset: *mut c_void, flag: c_uint) -> c_int;
        fn acl_free(object: *mut c_void) -> c_int;
        fn mbr_uuid_to_id(uuid: *const u8, id: *mut u32, id_type: *mut c_int) -> c_int;
    }

    /// What libSystem returned to be freed with `acl_free`: an ACL, or an entry's qualifier.
    struct Freed(*mut c_void);

    impl Drop for Freed {
        fn drop(&mut self) {
            // SAFETY: a non-null object libSystem allocated for `acl_free`, freed only here.
            unsafe { acl_free(self.0) };
        }
    }

    /// A call's result: `0` success, `-1` the error it left in `errno` ([`answer::status`]).
    fn checked(result: c_int) -> io::Result<()> {
        answer::status(result, io::Error::last_os_error)
    }

    /// The allow entries of the open `file`'s ACL; none without one (`ENOENT`: the descriptor is open, so it is the
    /// ACL that is missing, never the file). Whatever else fails while it is read, the walk or any part of an entry,
    /// is an error, never an entry, right or flag fewer: a missed one could only make the file look safer than it is.
    pub(super) fn allowed(file: &File) -> io::Result<Vec<Allowed>> {
        // SAFETY: the descriptor stays open for the call (borrowed from `file`).
        let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            return match Errno::from_io_error(&error) {
                Some(Errno::NOENT) => Ok(Vec::new()),
                _ => Err(error),
            };
        }
        let acl = Freed(acl);
        // Past the last entry `acl_get_entry` fails with `EINVAL`, as it does for an invalid ACL: validated first, and
        // asked only for the first and next entries, the ACL leaves the end as the only `EINVAL` ([`answer::entry`]).
        // SAFETY: `acl` is live.
        checked(unsafe { acl_valid(acl.0) })?;
        let mut allowed = Vec::new();
        let mut which = ACL_FIRST_ENTRY;
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: `acl` is live; the call only writes `entry`.
            let result = unsafe { acl_get_entry(acl.0, which, &mut entry) };
            if !answer::entry(result, io::Error::last_os_error)? {
                break;
            }
            let entry = answer::present(entry, "entry")?;
            which = ACL_NEXT_ENTRY;
            let mut tag = 0;
            // SAFETY: `entry` belongs to `acl`, live for the whole walk; so do its permission and flag sets.
            checked(unsafe { acl_get_tag_type(entry, &mut tag) })?;
            if tag != ACL_EXTENDED_ALLOW {
                continue;
            }
            let who = who(entry)?;
            let mut permset = ptr::null_mut();
            // SAFETY: as above.
            checked(unsafe { acl_get_permset(entry, &mut permset) })?;
            let permset = answer::present(permset, "permission set")?;
            let mut perms = Perm::empty();
            for perm in Perm::all().iter() {
                // SAFETY: as above.
                let result = unsafe { acl_get_perm_np(permset, perm.bits()) };
                if answer::bit(result, io::Error::last_os_error)? {
                    perms |= perm;
                }
            }
            let mut flagset = ptr::null_mut();
            // SAFETY: as above.
            checked(unsafe { acl_get_flagset_np(entry, &mut flagset) })?;
            let flagset = answer::present(flagset, "flag set")?;
            let mut flags = Flag::empty();
            for flag in Flag::all().iter() {
                // SAFETY: as above.
                let result = unsafe { acl_get_flag_np(flagset, flag.bits()) };
                if answer::bit(result, io::Error::last_os_error)? {
                    flags |= flag;
                }
            }
            allowed.push(Allowed { who, perms, flags });
        }
        Ok(allowed)
    }

    /// Who an extended entry names: its qualifier is a GUID (`guid_t`, 16 bytes), mapped to a uid or gid; one this
    /// machine cannot map (`ENOENT`) is nobody it knows.
    fn who(entry: *mut c_void) -> io::Result<Who> {
        // SAFETY: `entry` is a live entry of an extended ACL; the copy returned is ours to free.
        let guid = unsafe { acl_get_qualifier(entry) };
        if guid.is_null() {
            return Err(io::Error::last_os_error());
        }
        let guid = Freed(guid);
        let (mut id, mut kind) = (0, 0);
        // SAFETY: `guid` points at a live 16-byte `guid_t`; the call only writes the two outputs.
        match unsafe { mbr_uuid_to_id(guid.0.cast(), &mut id, &mut kind) } {
            0 if kind == ID_TYPE_UID => Ok(Who::User(id)),
            0 if kind == ID_TYPE_GID => Ok(Who::Group(id)),
            0 => Ok(Who::Unknown),
            error if error == Errno::NOENT.raw_os_error() => Ok(Who::Unknown),
            error => Err(io::Error::from_raw_os_error(error)),
        }
    }
}

/// What a libSystem ACL call answered ([`acl_by_fd`]), decided apart from the calls so it is tested anywhere. Only an
/// answer a call documents is a result; any other is an error, so nothing failed or unexpected reads as "not granted".
#[cfg(any(target_os = "macos", test))]
mod answer {
    use rustix::io::Errno;
    use std::ffi::c_int;
    use std::io;

    /// A call returning `0` on success, `-1` with the error in `errno` (`error`) on failure.
    pub(super) fn status(result: c_int, error: impl FnOnce() -> io::Error) -> io::Result<()> {
        match result {
            0 => Ok(()),
            -1 => Err(error()),
            other => Err(unexpected(other)),
        }
    }

    /// `acl_get_entry`: `0` an entry (`true`); `-1` with `EINVAL` past the last one of a valid ACL (`false`); any other
    /// failure an error.
    pub(super) fn entry(result: c_int, error: impl FnOnce() -> io::Error) -> io::Result<bool> {
        match status(result, error) {
            Ok(()) => Ok(true),
            Err(error) if result == -1 && Errno::from_io_error(&error) == Some(Errno::INVAL) => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// `acl_get_perm_np`, `acl_get_flag_np`: `1` set, `0` not set, `-1` with the error in `errno` (`error`) neither.
    pub(super) fn bit(result: c_int, error: impl FnOnce() -> io::Error) -> io::Result<bool> {
        match result {
            1 => Ok(true),
            0 => Ok(false),
            -1 => Err(error()),
            other => Err(unexpected(other)),
        }
    }

    /// What a successful call wrote (`what`), never null.
    pub(super) fn present<T>(pointer: *mut T, what: &str) -> io::Result<*mut T> {
        if pointer.is_null() {
            Err(io::Error::other(format!("an ACL call gave a null {what}")))
        } else {
            Ok(pointer)
        }
    }

    fn unexpected(result: c_int) -> io::Error {
        io::Error::other(format!("an ACL call returned {result}"))
    }
}

/// That a record created in the directory `dir` is given nothing for anybody untrusted on creation, before any byte of
/// it is written. macOS: a file created in a directory takes the directory's entries marked for files to inherit
/// (`file_inherit`), whatever its mode, so an allow entry so marked for anybody untrusted, of any right (reading too),
/// is refused. The directory itself may still be fine for others to read: only what would be made in it is refused.
#[cfg(target_os = "macos")]
fn check_inherited(dir: &File) -> io::Result<()> {
    if granted(Object::Open(dir))?
        .iter()
        .any(|granted| granted.flags.contains(exacl::Flag::FILE_INHERIT) && !granted.trusted)
    {
        return Err(super::denied(
            "its ACL passes access to what is created in it on to others",
        ));
    }
    Ok(())
}

/// Linux: a default ACL's entries are bounded by the mode a record is created with (`0600`: its mask, the group bits,
/// gives nobody named anything), so nothing it passes down reaches others. Other Unix: no ACL is read.
#[cfg(not(target_os = "macos"))]
fn check_inherited(_dir: &File) -> io::Result<()> {
    Ok(())
}

/// The uid an ACL entry names (macOS): an account's name, else a decimal uid; `None` for anything else (a GUID).
#[cfg(target_os = "macos")]
fn account_id(name: &str) -> io::Result<Option<u32>> {
    Ok(match Account::from_name(name)? {
        Some(account) => Some(account.uid.as_raw()),
        None => name.parse().ok(),
    })
}

/// The gid an ACL entry names (macOS), as [`account_id`].
#[cfg(target_os = "macos")]
fn group_id(name: &str) -> io::Result<Option<u32>> {
    Ok(match Group::from_name(name)? {
        Some(group) => Some(group.gid.as_raw()),
        None => name.parse().ok(),
    })
}

/// Other Unix: an ACL is not read, so a group's write bit (a POSIX.1e ACL's mask) is somebody untrusted writing.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn acl_others(_object: Object<'_>, mode: u32) -> io::Result<Others> {
    Ok(if mode & 0o020 != 0 {
        Others::Change
    } else {
        Others::Nothing
    })
}

/// Why a directory on the way (or a program) `object` with owner `uid`, group `gid` and `mode` is unsafe, `None` when
/// it is not: its owner is not trusted ([`trusted_owner`]), or somebody untrusted may write in it: through its extended
/// ACL ([`acl_others`]), or its mode ([`others_can_write`]), which is harmless only under the sticky bit (`/tmp`),
/// where nobody can remove or rename what is not theirs.
fn untrusted(
    uid: u32,
    gid: u32,
    mode: impl Into<u32>,
    object: Object<'_>,
) -> io::Result<Option<&'static str>> {
    let mode = mode.into();
    Ok(if !trusted_owner(uid)? {
        Some("owned by another user")
    } else if acl_others(object, mode)? == Others::Change
        || (mode & 0o1000 == 0 && others_can_write(gid, mode)?)
    {
        Some("others can replace what is in it")
    } else {
        None
    })
}

/// A directory on the way to something private, the entry itself ([`untrusted`]). A link here is refused: links are
/// followed before the checks (`super::resolve_links`), so one that is still on the way was put there meanwhile.
pub(super) fn check_ancestor(dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    let why = if metadata.is_symlink() {
        Some("a link on the way")
    } else {
        untrusted(
            metadata.uid(),
            metadata.gid(),
            metadata.mode(),
            Object::Entry(dir),
        )?
    };
    match why {
        Some(why) => Err(unsafe_entry(dir, why)),
        None => Ok(()),
    }
}

/// The directory a root stands for (`super::resolve_links`): none on Unix, whose one root `/` is itself.
pub(super) fn backing_directory(_root: &Path) -> io::Result<Option<PathBuf>> {
    Ok(None)
}

/// A link on the way, in a directory that passed [`check_ancestor`]: followed only when its owner is trusted too
/// ([`trusted_owner`]), as under a sticky bit whoever owns a link may replace it. A link's own mode means nothing.
pub(super) fn check_link(link: &Path) -> io::Result<()> {
    if !trusted_owner(fs::symlink_metadata(link)?.uid())? {
        return Err(unsafe_entry(link, "a link of another user's"));
    }
    Ok(())
}

fn unsafe_entry(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        ErrorKind::PermissionDenied,
        format!("unsafe directory {}: {why}", path.display()),
    )
}

/// Writes `bytes` as `path` as `how` says ([`Publish`]), every step relative to one handle open on its directory, so
/// the directory that is judged, written in, renamed in, cleaned up and synced is the one opened, whatever its path
/// names meanwhile. For a private directory that handle is opened without following a link and judged
/// (`super::judge_dir`) before anything is created through it. The staging file is created there (`openat`,
/// `O_EXCL | O_NOFOLLOW`, mode `0600`) under the name `tempfile` picks, written, given `path`'s mode for
/// `Publish::KeepAccess`, synced, then renamed over the record (`renameat`) or, for `Publish::New`, linked to its
/// name (`linkat`, which replaces nothing) and its staging name removed (`unlinkat`) before the directory is synced
/// (`fsync`). On any failure the staging name is removed.
pub(super) fn write_whole(path: &Path, bytes: &[u8], how: Publish) -> io::Result<()> {
    let path = &super::record_path(path, how)?;
    let dir_path = super::parent_of(path)?;
    let dir = open_dir(dir_path, how.judged(), false)?;
    if how.judged() {
        super::judge_dir::<Os>(dir_path, &dir)?;
    }
    write_whole_at(&dir, path, bytes, how, || Ok(()))
}

/// The single atomic writer, also used by a held directory. `path` supplies only the staging label and leaf name;
/// every filesystem operation is relative to `dir` (including cleanup if the directory was renamed meanwhile). Nothing
/// is staged in a directory that would pass access to others on to what is created in it ([`check_inherited`]).
fn write_whole_at(
    dir: &File,
    path: &Path,
    bytes: &[u8],
    how: Publish,
    before_publish: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let dir_path = super::parent_of(path)?;
    let name = path.file_name().unwrap_or_default();
    check_inherited(dir).map_err(|error| super::refused(dir_path, error))?;
    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    // `tempfile` only names it: what it would remove on drop is removed below, relative to `dir`.
    let (mut file, staged) = tempfile::Builder::new()
        .prefix(&super::staging_prefix(path))
        .rand_bytes(super::STAGING_RANDOM)
        .make_in(dir_path, |temp| {
            let temp = temp.file_name().unwrap_or_default();
            Ok(File::from(rfs::openat(
                dir,
                temp,
                create,
                Mode::from_raw_mode(0o600),
            )?))
        })?
        .keep()
        .map_err(|error| error.error)?;
    let staged = staged.file_name().unwrap_or_default();
    let written = (|| {
        file.write_all(bytes)?;
        if how == Publish::KeepAccess {
            match rfs::statat(dir, name, AtFlags::empty()) {
                Ok(record) => rfs::fchmod(&file, Mode::from_raw_mode(record.st_mode & 0o7777))?,
                Err(Errno::NOENT) => (),
                Err(error) => return Err(error.into()),
            }
        }
        file.sync_all()?;
        before_publish()?;
        match how {
            Publish::New => rfs::linkat(dir, staged, dir, name, AtFlags::empty()),
            _ => rfs::renameat(dir, staged, dir, name),
        }
        .map_err(io::Error::from)
    })();
    if written.is_err() || how == Publish::New {
        let removed = rfs::unlinkat(dir, staged, AtFlags::empty());
        // Gone already after a publication: a reader finishing the creation (`finish_creation`) removed it first.
        if written.is_ok() && removed != Err(Errno::NOENT) {
            removed?;
        }
    }
    written?;
    rfs::fsync(dir).map_err(io::Error::from)
}

/// The held directory capability; public operations are declared in the shared interfaces.
pub struct Held(pub(crate) HeldState);

impl Held {
    fn open_with(path: &Path, access: Option<Access>) -> io::Result<Self> {
        // Links were followed where trusted (`super::resolve_links`): every step is opened without following one.
        let paths = super::real_directory_paths(path)?;
        let mut chain: Vec<(std::path::PathBuf, File)> = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            let leaf = index + 1 == paths.len();
            let search_only = !leaf || access.is_none();
            let file = if let Some((_, parent)) = chain.last() {
                open_subdirectory(parent, path, search_only)?
            } else {
                open_dir(path, true, search_only)?
            };
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

    fn open_identified(path: &Path, access: Access, expected: &FileId) -> io::Result<Self> {
        let held = Self::open_with(path, Some(access))?;
        held.0.identified(path, expected)?;
        Ok(held)
    }

    fn identity(&self) -> io::Result<FileId> {
        <Os as super::Fs>::identity(self.0.leaf().1)
    }

    fn open_beneath(&self, relative: &Path, access: Access) -> io::Result<Self> {
        self.0.beneath(relative, access).map(Held)
    }

    fn read(&self, name: &OsStr, access: Access, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let name = super::child_name(name)?;
        self.0.verify()?;
        let (path, dir) = self.0.leaf();
        let Some(file) = open_record(dir, name, access)? else {
            return Ok(None);
        };
        let bytes = super::read_bounded(file, &path.join(name), limit)?;
        self.0.verify()?;
        Ok(Some(bytes))
    }

    fn write(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
        let name = super::child_name(name)?;
        self.0.verify()?;
        let (path, dir) = self.0.leaf();
        open_record(dir, name, Access::NotWritableByOthers)?;
        write_whole_at(dir, &path.join(name), bytes, Publish::Vouched, || {
            self.0.before_publish()
        })
    }

    fn remove(&self, name: &OsStr) -> io::Result<()> {
        let name = super::child_name(name)?;
        self.0.verify()?;
        let (_, dir) = self.0.leaf();
        if open_record(dir, name, Access::NotWritableByOthers)?.is_none() {
            return Ok(());
        }
        match rfs::unlinkat(dir, name, AtFlags::empty()) {
            Ok(()) => rfs::fsync(dir).map_err(Into::into),
            Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// The directory named by `path`'s last component, opened relative to `parent` (`openat`) without following a link
/// there (`O_NOFOLLOW | O_DIRECTORY`: a link, or anything but a directory, is refused).
pub(super) fn open_subdirectory(parent: &File, path: &Path, search_only: bool) -> io::Result<File> {
    let name = path
        .file_name()
        .ok_or_else(|| super::denied("invalid directory component"))?;
    let flags =
        directory_access(search_only) | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    rfs::openat(parent, name, flags, Mode::empty())
        .map(File::from)
        .map_err(|error| directory_error(path, error.into()))
}

fn open_record(dir: &File, name: &Path, access: Access) -> io::Result<Option<File>> {
    let Some(file) = open_child(dir, name)? else {
        return Ok(None);
    };
    super::judge_access(&file, access, false)?;
    Ok(Some(file))
}

// One no-follow child opener for records and programs, relative to the checked directory.
fn open_child(dir: &File, name: &Path) -> io::Result<Option<File>> {
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let file = match rfs::openat(dir, name, flags, Mode::empty()) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => {
            // Some special files cannot be opened at all (a socket gives ENXIO on Linux). Classify those by
            // no-follow metadata relative to the held directory; never turn a failed open into a usable record.
            if rfs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|stat| {
                rfs::FileType::from_raw_mode(stat.st_mode) != rfs::FileType::RegularFile
            }) {
                return Err(super::refused_file(name, "not a regular record"));
            }
            return Err(directory_error(name, error.into()));
        }
    };
    Ok(Some(file))
}

fn directory_error(path: &Path, error: io::Error) -> io::Error {
    if matches!(error.raw_os_error(), Some(code) if code == Errno::LOOP.raw_os_error() || code == Errno::NOTDIR.raw_os_error())
    {
        super::refused(
            path,
            "not an ordinary directory or record; choose an unredirected data directory",
        )
    } else {
        error
    }
}

/// Which mount a handle is on. Linux: the mount ID from `/proc/self/fdinfo`, which tells bind mounts apart even on
/// one device (kept over `statx`'s `STATX_MNT_ID` for kernels before 5.8); macOS: `fstatfs`'s mounted-at path.
/// Shared by held descent and removal; an unavailable probe fails closed.
#[derive(PartialEq, Debug)]
pub(crate) struct Mount(
    #[cfg(target_os = "linux")] u64,
    #[cfg(target_os = "macos")] Vec<i8>,
);

#[cfg(target_os = "linux")]
pub(crate) fn mount_of(file: &File) -> io::Result<Mount> {
    use std::os::fd::AsRawFd;
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .and_then(|id| id.trim().parse().ok())
        .map(Mount)
        .ok_or_else(|| super::denied("cannot inspect the mount"))
}

#[cfg(target_os = "macos")]
pub(crate) fn mount_of(file: &File) -> io::Result<Mount> {
    Ok(Mount(rfs::fstatfs(file)?.f_mntonname.to_vec()))
}

pub(super) fn reopen_directory(path: &Path) -> io::Result<File> {
    open_dir(path, true, true)
}

/// Judge the directory actually held ([`untrusted`]).
pub(super) fn check_ancestor_handle(path: &Path, file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_dir()
        || untrusted(
            metadata.uid(),
            metadata.gid(),
            metadata.mode(),
            Object::Open(file),
        )?
        .is_some()
    {
        return Err(super::refused(path, "unsafe ancestor owner or permissions"));
    }
    Ok(())
}

/// Finishes the creation of the record at `path` its writer (`write_whole`, `Publish::New`) left with its staging
/// name too (`Fs::open_created_file`), every step relative to one handle on its directory, opened without following a
/// link: only when that directory is private, the record in it is a regular file of this user's open to nobody else
/// with exactly two names, and an entry named exactly as a staging name of it (`super::is_staging_name`) is that same
/// file, is that entry removed (`unlinkat`) and the directory synced. Anything else is left as it is, for the read
/// that follows to judge.
pub(super) fn finish_creation(path: &Path) -> io::Result<()> {
    let Ok(path) = super::record_path(path, Publish::New) else {
        return Ok(());
    };
    let (Ok(dir_path), Some(name)) = (super::parent_of(&path), path.file_name()) else {
        return Ok(());
    };
    let Ok(dir) = open_dir(dir_path, true, false) else {
        return Ok(());
    };
    if super::judge_dir::<Os>(dir_path, &dir).is_err() {
        return Ok(());
    }
    let read =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let Ok(record) = rfs::openat(&dir, name, read, Mode::empty()).map(File::from) else {
        return Ok(());
    };
    let metadata = record.metadata()?;
    let stat = rfs::fstat(&record)?;
    if !metadata.is_file()
        || metadata.nlink() != 2
        || !UserId::current().owns_file(&record)?
        || <Os as super::Fs>::others_of(&record)? != Others::Nothing
    {
        return Ok(());
    }
    for entry in rfs::Dir::read_from(&dir)? {
        let entry = entry?;
        let staged = OsStr::from_bytes(entry.file_name().to_bytes());
        if !super::is_staging_name(name, staged) {
            continue;
        }
        // A name gone meanwhile was removed by another reader finishing the same creation, or by the writer itself:
        // not an error (never `NotFound`, which would read as no record), the read that follows judges the record.
        let found = match rfs::statat(&dir, staged, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(found) => found,
            Err(Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if found.st_dev == stat.st_dev && found.st_ino == stat.st_ino {
            match rfs::unlinkat(&dir, staged, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => (),
                Err(error) => return Err(error.into()),
            }
            return rfs::fsync(&dir).map_err(io::Error::from);
        }
    }
    Ok(())
}

/// A handle on the directory `path`, for `*at` calls; with `no_link`, a link there is refused, not followed.
fn open_dir(path: &Path, no_link: bool, search_only: bool) -> io::Result<File> {
    let no_link = if no_link {
        OFlags::NOFOLLOW
    } else {
        OFlags::empty()
    };
    let flags = directory_access(search_only) | OFlags::DIRECTORY | OFlags::CLOEXEC | no_link;
    Ok(File::from(rfs::open(path, flags, Mode::empty())?))
}

// Ancestors, program validation and ancestry rechecks never list directory entries. Record leaf directories
// retain read-capable handles where their writer needs fsync. macOS O_SEARCH is O_EXEC | O_DIRECTORY (macOS 12+).
fn directory_access(search_only: bool) -> OFlags {
    if !search_only {
        return OFlags::RDONLY;
    }
    #[cfg(target_os = "linux")]
    {
        OFlags::PATH
    }
    #[cfg(target_os = "macos")]
    {
        OFlags::from_bits_retain(libc::O_SEARCH as _)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        OFlags::RDONLY
    }
}

fn flags(flags: OFlags) -> i32 {
    flags.bits() as i32
}

/// Unix-only test fixtures: an open socket and a FIFO, neither of which is a record.
#[cfg(test)]
pub(super) fn special_records(dir: &Path) -> std::os::unix::net::UnixListener {
    let socket = std::os::unix::net::UnixListener::bind(dir.join("socket")).unwrap();
    // rustix has no mkfifo on macOS; the POSIX utility provides the fixture on both Unix hosts.
    assert!(std::process::Command::new("mkfifo")
        .args(["-m", "600"])
        .arg(dir.join("fifo"))
        .status()
        .unwrap()
        .success());
    socket
}

#[cfg(any(test, feature = "test-support"))]
impl super::TestFiles for Os {
    fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    fn make_executable(path: &Path) {
        set_mode(path, |mode| mode | 0o700);
    }

    fn let_others_read(path: &Path) {
        set_mode(path, |mode| mode | 0o055);
    }

    fn let_others_change(path: &Path) {
        set_mode(path, |mode| mode | 0o077);
    }

    fn shut_others_out(path: &Path) {
        set_mode(path, |mode| mode & !0o077);
    }
}

/// `path`'s permission bits changed by `change` (tests).
#[cfg(any(test, feature = "test-support"))]
fn set_mode(path: &Path, change: impl Fn(u32) -> u32) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).unwrap().mode() & 0o7777;
    fs::set_permissions(path, fs::Permissions::from_mode(change(mode))).unwrap();
}

/// Who an extended ACL is given to (tests): the account `nobody`, or its primary group; neither is trusted.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) enum Grantee {
    User,
    Group,
}

/// Gives `whom` ([`Grantee`]) access to `path` through an extended ACL only, the owner's mode bits kept and the owning
/// group and everybody else given nothing (tests): with `write`, every change; without, reading (and searching).
/// Linux: the access ACL written as the kernel's `system.posix_acl_access` attribute, as `setfacl` does (which a host
/// may not have); macOS: `chmod +a`.
#[cfg(test)]
pub(super) fn grant_by_acl(path: &Path, whom: Grantee, write: bool) {
    acl_for_nobody(path, whom, write, false);
}

/// Has the directory `dir` pass access on to `whom` ([`Grantee`]) in what is created inside it, through an extended
/// ACL only, `dir` itself granting them nothing (tests): with `write`, reading and writing; without, reading. Linux:
/// a default ACL (`system.posix_acl_default`); macOS: `chmod +a` with `file_inherit,only_inherit`.
#[cfg(test)]
pub(super) fn pass_down_by_acl(dir: &Path, whom: Grantee, write: bool) {
    acl_for_nobody(dir, whom, write, true);
}

/// [`grant_by_acl`], or with `pass_down` [`pass_down_by_acl`].
#[cfg(test)]
fn acl_for_nobody(path: &Path, whom: Grantee, write: bool, pass_down: bool) {
    let nobody = Account::from_name("nobody")
        .unwrap()
        .expect("a nobody account");
    assert!(!trusted_owner(nobody.uid.as_raw()).unwrap() && !trusted_group(nobody.gid.as_raw()));
    #[cfg(target_os = "linux")]
    {
        let mode = fs::symlink_metadata(path).unwrap().mode();
        let granted: u16 = if write { 0o7 } else { 0o5 };
        let (tag, id) = match whom {
            Grantee::User => (0x02, nobody.uid.as_raw()),
            Grantee::Group => (0x08, nobody.gid.as_raw()),
        };
        // `posix_acl_xattr.h`: version 2, then (tag, permissions, id) entries ordered by tag; owner, owning group,
        // mask and other have no id.
        let mut entries = [
            (0x01u16, ((mode >> 6) & 0o7) as u16, u32::MAX),
            (tag, granted, id),
            (0x04, 0, u32::MAX),
            (0x10, granted, u32::MAX),
            (0x20, 0, u32::MAX),
        ];
        entries.sort();
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in entries {
            acl.extend(tag.to_le_bytes());
            acl.extend(perm.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        rfs::lsetxattr(
            path,
            if pass_down {
                "system.posix_acl_default"
            } else {
                "system.posix_acl_access"
            },
            &acl,
            rfs::XattrFlags::empty(),
        )
        .unwrap();
    }
    #[cfg(target_os = "macos")]
    {
        let principal = match whom {
            Grantee::User => format!("user:{}", nobody.name),
            Grantee::Group => format!(
                "group:{}",
                Group::from_gid(nobody.gid).unwrap().unwrap().name
            ),
        };
        let rights = match (fs::metadata(path).unwrap().is_dir() && !pass_down, write) {
            (true, true) => "list,search,add_file,add_subdirectory,delete_child",
            (true, false) => "list,search",
            (false, true) => "read,write,append",
            (false, false) => "read",
        };
        let status = std::process::Command::new("chmod")
            .arg("+a")
            .arg(if pass_down {
                format!("{principal} allow {rights},file_inherit,only_inherit")
            } else {
                format!("{principal} allow {rights}")
            })
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "chmod +a");
    }
}

#[cfg(test)]
pub(super) fn system_program() -> PathBuf {
    PathBuf::from("/bin/sh")
}

/// The groups this user belongs to, by what the policy makes of them (tests): an admin group, this user's own private
/// group, and one that is neither; `None` where this host gives this user no such group. `id -G` lists them: `nix`
/// has no `getgroups` on macOS.
#[cfg(test)]
pub(super) struct TestGroups {
    pub(super) admin: Option<u32>,
    pub(super) private: Option<u32>,
    pub(super) other: Option<u32>,
}

#[cfg(test)]
pub(super) fn test_groups() -> TestGroups {
    let output = std::process::Command::new("id").arg("-G").output().unwrap();
    assert!(output.status.success(), "id -G");
    let mut groups = TestGroups {
        admin: None,
        private: None,
        other: None,
    };
    for gid in String::from_utf8(output.stdout).unwrap().split_whitespace() {
        let gid: u32 = gid.parse().unwrap();
        let Some(group) = Group::from_gid(Gid::from_raw(gid)).unwrap() else {
            continue;
        };
        let slot = if ADMIN_GROUPS.contains(&group.name.as_str()) {
            &mut groups.admin
        } else if trusted_group(gid) {
            &mut groups.private
        } else {
            &mut groups.other
        };
        slot.get_or_insert(gid);
    }
    groups
}

// Unix only: accounts and groups are Unix's. The policy is phrased on the records the system's databases give, built
// here, since another administrator's account cannot be made in a test.
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn errno(errno: rustix::io::Errno) -> impl FnOnce() -> io::Error {
        move || io::Error::from_raw_os_error(errno.raw_os_error())
    }

    #[test]
    fn an_acl_call_failing_or_answering_what_it_never_does_is_an_error() {
        use rustix::io::Errno;
        assert!(answer::status(0, errno(Errno::INVAL)).is_ok());
        assert!(answer::status(-1, errno(Errno::NOMEM)).is_err());
        assert!(answer::status(1, errno(Errno::INVAL)).is_err());
    }

    #[test]
    fn only_einval_ends_the_entries_any_other_failure_is_an_error() {
        use rustix::io::Errno;
        assert!(answer::entry(0, errno(Errno::INVAL)).unwrap());
        assert!(!answer::entry(-1, errno(Errno::INVAL)).unwrap());
        assert!(answer::entry(-1, errno(Errno::NOMEM)).is_err());
        assert!(answer::entry(-1, errno(Errno::NOENT)).is_err());
        assert!(answer::entry(1, errno(Errno::INVAL)).is_err());
        assert!(answer::entry(-2, errno(Errno::INVAL)).is_err());
    }

    #[test]
    fn a_permission_or_flag_is_set_not_set_or_an_error_never_not_set_by_failing() {
        use rustix::io::Errno;
        assert!(answer::bit(1, errno(Errno::INVAL)).unwrap());
        assert!(!answer::bit(0, errno(Errno::INVAL)).unwrap());
        assert!(answer::bit(-1, errno(Errno::INVAL)).is_err());
        assert!(answer::bit(2, errno(Errno::INVAL)).is_err());
    }

    #[test]
    fn a_null_entry_or_set_from_a_successful_call_is_an_error() {
        let mut value = 0u8;
        assert!(answer::present(&raw mut value, "entry").is_ok());
        assert!(answer::present(std::ptr::null_mut::<u8>(), "entry").is_err());
    }

    fn group(name: &str, gid: u32, members: &[&str]) -> Group {
        Group {
            name: name.into(),
            passwd: CString::default(),
            gid: Gid::from_raw(gid),
            mem: members.iter().map(|member| member.to_string()).collect(),
        }
    }

    #[test]
    fn an_administrator_is_in_an_admin_group_by_its_primary_group_or_its_membership() {
        let admins = [group("admin", 80, &["root", "ana"]), group("wheel", 0, &[])];
        assert!(
            is_administrator("ana", Gid::from_raw(20), &admins),
            "listed"
        );
        assert!(
            is_administrator("svc", Gid::from_raw(0), &admins),
            "primary"
        );
        assert!(!is_administrator("bob", Gid::from_raw(20), &admins));
        assert!(
            !is_administrator("ana", Gid::from_raw(20), &[]),
            "no admin group here"
        );
    }

    #[test]
    fn a_private_group_is_named_as_its_account_is_its_primary_group_and_lists_nobody_else() {
        let ana = Account::from_uid(Uid::effective()).unwrap().unwrap();
        let ana = Account {
            name: "ana".into(),
            gid: Gid::from_raw(1000),
            ..ana
        };
        assert!(is_private_group(&group("ana", 1000, &[]), &ana));
        assert!(is_private_group(&group("ana", 1000, &["ana"]), &ana));
        assert!(
            !is_private_group(&group("staff", 1000, &[]), &ana),
            "another name"
        );
        assert!(
            !is_private_group(&group("ana", 1001, &[]), &ana),
            "not its primary group"
        );
        assert!(
            !is_private_group(&group("ana", 1000, &["ana", "bob"]), &ana),
            "another member"
        );
        assert!(
            !is_private_group(&group("ana", 1000, &["bob"]), &ana),
            "another member alone"
        );
    }

    #[test]
    fn the_admin_groups_are_trusted_to_write_and_root_and_this_user_to_own() {
        for name in ADMIN_GROUPS {
            if let Some(group) = Group::from_name(name).unwrap() {
                assert!(trusted_group(group.gid.as_raw()), "{name}");
            }
        }
        assert!(trusted_owner(0).unwrap());
        assert!(trusted_owner(rustix::process::geteuid().as_raw()).unwrap());
        // `nobody` is in no admin group wherever it exists.
        if let Some(nobody) = Account::from_name("nobody").unwrap() {
            assert!(!trusted_owner(nobody.uid.as_raw()).unwrap());
            assert!(!trusted_group(nobody.gid.as_raw()));
        }
    }
}
