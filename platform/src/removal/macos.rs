//! macOS mount identity (`crate::fs::mount_of`, rustix fstatfs); never open a FIFO/device to inspect it.
use rustix::fs::{self as rfs, FileType, Mode, OFlags};
use std::ffi::OsStr;
use std::fs::File;
use std::io;

pub(super) use crate::fs::{mount_of as of, Mount};

pub(super) fn entry(parent: &File, name: &OsStr, before: &rfs::Stat) -> io::Result<Option<File>> {
    if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile {
        return Ok(None);
    }
    Ok(Some(File::from(rfs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?)))
}
