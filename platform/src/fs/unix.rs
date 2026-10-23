//! Unix: owner and mode bits, `O_NOFOLLOW`, the descriptor judged as opened. No `unsafe` of ours.

use super::Create;
use std::fs::{DirBuilder, File, OpenOptions};
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
