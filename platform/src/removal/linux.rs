//! Linux mount IDs distinguish bind mounts even on the same device. Retain connector's /proc probe for old kernels.
use rustix::fs::{self as rfs, Mode, OFlags};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

#[derive(PartialEq)]
pub(super) struct Mount(u64);

pub(super) fn of(file: &File) -> io::Result<Mount> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .and_then(|id| id.trim().parse().ok())
        .map(Mount)
        .ok_or_else(|| super::super::refused("cannot inspect removal mount"))
}

pub(super) fn entry(parent: &File, name: &OsStr, _: &rfs::Stat) -> io::Result<Option<File>> {
    Ok(Some(File::from(rfs::openat(
        parent,
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)))
}
