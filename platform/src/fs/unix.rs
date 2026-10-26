//! Unix: owner and mode bits, `O_NOFOLLOW`, the descriptor judged as opened, a record written relative to a handle on
//! its directory (`rustix`); who the administrators are, from the system's user and group databases (`nix`). No
//! `unsafe` of ours.
//!
//! Who is trusted besides this user: root and the administrators, the members of [`ADMIN_GROUPS`]. They may own what
//! is on the way to our files, and one of those groups, or this user's own private group, may write in it. Anybody else
//! able to write there (another group, everybody) makes it unsafe, unless the sticky bit keeps them to their own entries.

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
            || untrusted(metadata.st_uid, metadata.st_gid, metadata.st_mode)?.is_some()
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
        Ok(if others_can_write(metadata.gid(), mode)? {
            Others::Change
        } else if mode & 0o077 != 0 {
            Others::Read
        } else {
            Others::Nothing
        })
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
/// private group ([`is_private_group`]).
fn trusted_group(gid: u32) -> io::Result<bool> {
    let Some(group) = Group::from_gid(Gid::from_raw(gid))? else {
        return Ok(false);
    };
    if ADMIN_GROUPS.contains(&group.name.as_str()) {
        return Ok(true);
    }
    let Some(me) = Account::from_uid(Uid::effective())? else {
        return Ok(false);
    };
    Ok(is_private_group(&group, &me.name, me.gid))
}

/// Whether `group` is the private group of the account `name` whose primary group is `gid` (Linux's user private
/// groups, with umask `002`): named as the account, its primary group, listing nobody else. Whether another account
/// has it as its primary group is not visible from the group; the convention is that none does.
fn is_private_group(group: &Group, name: &str, gid: Gid) -> bool {
    group.gid == gid && group.name == name && group.mem.iter().all(|member| member == name)
}

/// Whether somebody untrusted may write in what has group `gid` and permission bits `mode`: everybody (`o+w`), or a
/// group that is not trusted ([`trusted_group`]). The sticky bit is the caller's to weigh.
fn others_can_write(gid: u32, mode: u32) -> io::Result<bool> {
    Ok(mode & 0o002 != 0 || (mode & 0o020 != 0 && !trusted_group(gid)?))
}

/// Why a directory on the way (or a program) with owner `uid`, group `gid` and `mode` is unsafe, `None` when it is
/// not: its owner is not trusted ([`trusted_owner`]), or somebody untrusted may write in it ([`others_can_write`]),
/// which is harmless only under the sticky bit (`/tmp`), where nobody can remove or rename what is not theirs.
fn untrusted(uid: u32, gid: u32, mode: impl Into<u32>) -> io::Result<Option<&'static str>> {
    let mode = mode.into();
    Ok(if !trusted_owner(uid)? {
        Some("owned by another user")
    } else if mode & 0o1000 == 0 && others_can_write(gid, mode)? {
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
        untrusted(metadata.uid(), metadata.gid(), metadata.mode())?
    };
    match why {
        Some(why) => Err(unsafe_entry(dir, why)),
        None => Ok(()),
    }
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
/// every filesystem operation is relative to `dir` (including cleanup if the directory was renamed meanwhile).
fn write_whole_at(
    dir: &File,
    path: &Path,
    bytes: &[u8],
    how: Publish,
    before_publish: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let dir_path = super::parent_of(path)?;
    let name = path.file_name().unwrap_or_default();
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
                File::from(
                    rfs::openat(
                        parent,
                        path.file_name()
                            .ok_or_else(|| super::denied("invalid directory component"))?,
                        directory_access(search_only)
                            | OFlags::DIRECTORY
                            | OFlags::CLOEXEC
                            | OFlags::NOFOLLOW,
                        Mode::empty(),
                    )
                    .map_err(|error| directory_error(path, error.into()))?,
                )
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

pub(super) fn reopen_directory(path: &Path) -> io::Result<File> {
    open_dir(path, true, true)
}

/// Judge the directory actually held ([`untrusted`]).
pub(super) fn check_ancestor_handle(path: &Path, file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() || untrusted(metadata.uid(), metadata.gid(), metadata.mode())?.is_some() {
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
    let me = Account::from_uid(Uid::effective()).unwrap().unwrap();
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
        } else if is_private_group(&group, &me.name, me.gid) {
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
        let me = |group: &Group| is_private_group(group, "ana", Gid::from_raw(1000));
        assert!(me(&group("ana", 1000, &[])));
        assert!(me(&group("ana", 1000, &["ana"])));
        assert!(!me(&group("ana", 1000, &["ana", "bob"])), "another member");
        assert!(!me(&group("staff", 1000, &[])), "a shared primary group");
        assert!(!me(&group("ana", 1001, &[])), "not its primary group");
    }

    #[test]
    fn the_admin_groups_are_trusted_to_write_and_root_and_this_user_to_own() {
        for name in ADMIN_GROUPS {
            if let Some(group) = Group::from_name(name).unwrap() {
                assert!(trusted_group(group.gid.as_raw()).unwrap(), "{name}");
            }
        }
        assert!(trusted_owner(0).unwrap());
        assert!(trusted_owner(rustix::process::geteuid().as_raw()).unwrap());
        // `nobody` is in no admin group wherever it exists.
        if let Some(nobody) = Account::from_name("nobody").unwrap() {
            assert!(!trusted_owner(nobody.uid.as_raw()).unwrap());
            assert!(!trusted_group(nobody.gid.as_raw()).unwrap());
        }
    }
}
