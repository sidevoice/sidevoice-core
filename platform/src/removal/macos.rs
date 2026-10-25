//! macOS mount identity from the maintained rustix fstatfs binding; never open a FIFO/device to inspect it.
use rustix::fs::{self as rfs, FileType, Mode, OFlags};
use std::ffi::OsStr;
use std::fs::File;
use std::io;

#[derive(PartialEq)]
pub(super) struct Mount(Vec<i8>);

pub(super) fn of(file: &File) -> io::Result<Mount> {
    Ok(Mount(rfs::fstatfs(file)?.f_mntonname.to_vec()))
}

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
