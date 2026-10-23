//! Unix: owner and mode bits, `O_NOFOLLOW`, the descriptor judged as opened. No `unsafe` of ours.

use super::{Create, FileId, Others};
use crate::user::{User, UserId};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use rustix::fs::OFlags;

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

    fn sync_dir(dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    fn others_of(file: &File) -> io::Result<Others> {
        let mode = file.metadata()?.mode();
        Ok(if mode & 0o022 != 0 {
            Others::Change
        } else if mode & 0o055 != 0 {
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

/// A directory on the way to something private: the entry itself, this user's or root's; others writing is harmless
/// only under the sticky bit (`/tmp`), where nobody can remove or rename what is not theirs. A link is judged as a
/// link and trusted only when root's: where one of this user's leads is not judged, and the path through it is not
/// the one the checks saw.
pub(super) fn check_ancestor(dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    let unsafe_dir = |why: &str| {
        io::Error::new(
            ErrorKind::PermissionDenied,
            format!("unsafe directory {}: {why}", dir.display()),
        )
    };
    if metadata.is_symlink() && metadata.uid() != 0 {
        return Err(unsafe_dir("a link on the way"));
    }
    if metadata.uid() != 0 && !UserId::current().owns_path(dir)? {
        return Err(unsafe_dir("owned by another user"));
    }
    if metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
        return Err(unsafe_dir("others can replace what is in it"));
    }
    Ok(())
}

/// Gives the staged replacement of `path` the mode `path` has now, if it exists (`Fs::replace`).
pub(super) fn keep_access(path: &Path, staged: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(path) {
        Ok(metadata) => {
            staged.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o7777))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn flags(flags: OFlags) -> i32 {
    flags.bits() as i32
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
