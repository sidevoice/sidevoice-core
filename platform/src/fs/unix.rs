//! Unix: owner and mode bits, `O_NOFOLLOW`, the descriptor judged as opened. No `unsafe` of ours.

use super::Create;
use crate::user::{User, UserId};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
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
            Create::New => options.write(true).create_new(true),
            Create::Append => options.append(true).create(true),
            Create::ReadWrite => options.read(true).write(true).create(true).truncate(false),
        };
        options
            .mode(0o600)
            .custom_flags(flags(OFlags::NOFOLLOW))
            .open(path)
    }

    fn open_read(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(flags(OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY))
            .open(path)
    }

    fn open_to_others(file: &File) -> io::Result<bool> {
        Ok(file.metadata()?.mode() & 0o077 != 0)
    }

    fn sync_dir(dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }
}

/// A directory on the way to something private (the connector's rule): the entry itself; this user's or root's;
/// others writing is harmless only under the sticky bit (`/tmp`), where nobody can remove or rename what is not
/// theirs. A link is judged as a link and trusted only when root's: where one of this user's leads is not judged.
pub(super) fn trusted_ancestor(dir: &Path) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(dir)?;
    if metadata.is_symlink() && metadata.uid() != 0 {
        return Ok(false);
    }
    Ok((metadata.uid() == 0 || UserId::current().owns_path(dir)?)
        && (metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0))
}

fn flags(flags: OFlags) -> i32 {
    flags.bits() as i32
}

/// Lets group and others into `path` (tests).
#[cfg(test)]
pub(super) fn let_others_in(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().mode() | 0o077;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Makes `link` lead to the directory `target` (tests).
#[cfg(test)]
pub(super) fn link_dir(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// Makes `link` lead to the file `target` (tests).
#[cfg(test)]
pub(super) fn link_file(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}
