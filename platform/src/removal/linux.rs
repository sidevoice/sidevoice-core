//! Linux mount IDs distinguish bind mounts even on the same device (`crate::fs::mount_of`, the /proc probe).
use rustix::fs::{self as rfs, Mode, OFlags};
use std::ffi::OsStr;
use std::fs::File;
use std::io;

pub(super) use crate::fs::{mount_of as of, Mount};

pub(super) fn entry(parent: &File, name: &OsStr, _: &rfs::Stat) -> io::Result<Option<File>> {
    Ok(Some(File::from(rfs::openat(
        parent,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)))
}
