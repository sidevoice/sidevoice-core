//! Connector's descriptor-relative traversal, using safe rustix operations and the shared held-root checks.
use crate::fs::{Files, Fs, HeldState, Others};
use crate::user::{User, UserId};
use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod mount;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod mount;

struct Node<'a> {
    file: &'a File,
    parent: Option<(&'a Node<'a>, &'a OsStr)>,
}

impl Node<'_> {
    fn verify(&self, root: &HeldState) -> io::Result<()> {
        if let Some((parent, name)) = self.parent {
            parent.verify(root)?;
            same(
                &rfs::fstat(self.file)?,
                &rfs::statat(parent.file, name, AtFlags::SYMLINK_NOFOLLOW)?,
            )
        } else {
            root.verify()
        }
    }
}

fn same(a: &rfs::Stat, b: &rfs::Stat) -> io::Result<()> {
    if a.st_dev == b.st_dev
        && a.st_ino == b.st_ino
        && FileType::from_raw_mode(a.st_mode) == FileType::from_raw_mode(b.st_mode)
    {
        Ok(())
    } else {
        Err(super::refused("removal entry replaced"))
    }
}

pub(super) fn contents(root: &HeldState, allowed: &[&OsStr], remove: bool) -> io::Result<()> {
    for name in allowed {
        crate::fs::child_name(name)?;
    }
    root.verify()?;
    let node = Node {
        file: root.leaf().1,
        parent: None,
    };
    let expected = mount::of(node.file)?;
    for name in allowed {
        visit(root, &node, name, &expected, remove, 0)?;
    }
    root.verify()?;
    Ok(())
}

fn visit(
    root: &HeldState,
    parent: &Node<'_>,
    name: &OsStr,
    expected: &mount::Mount,
    remove: bool,
    depth: usize,
) -> io::Result<()> {
    if depth > 64 {
        return Err(super::refused("removal tree is too deep"));
    }
    parent.verify(root)?;
    let before = match rfs::statat(parent.file, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if before.st_dev != rfs::fstat(root.leaf().1)?.st_dev {
        return Err(super::refused("removal crosses a device boundary"));
    }
    let directory = FileType::from_raw_mode(before.st_mode) == FileType::Directory;
    let opened = if directory {
        Some(File::from(rfs::openat(
            parent.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    } else {
        mount::entry(parent.file, name, &before)?
    };
    if let Some(file) = &opened {
        same(&before, &rfs::fstat(file)?)?;
        if mount::of(file)? != *expected {
            return Err(super::refused("removal crosses a mount boundary"));
        }
        if directory {
            if !UserId::current().owns_file(file)?
                || matches!(Files::others_of(file)?, Others::Change)
            {
                return Err(super::refused(
                    "unsafe removal directory owner or permissions",
                ));
            }
            let node = Node {
                file,
                parent: Some((parent, name)),
            };
            // A fresh open has its own enumeration offset; dup would share the previous walk's offset.
            let entries = rfs::Dir::new(rfs::openat(
                file,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?)?;
            let names: Vec<OsString> = entries
                .map(|entry| {
                    entry.map(|entry| OsStr::from_bytes(entry.file_name().to_bytes()).to_owned())
                })
                .collect::<Result<_, _>>()?;
            for child in names {
                if child != "." && child != ".." {
                    visit(root, &node, &child, expected, remove, depth + 1)?;
                }
            }
            node.verify(root)?;
        }
    }
    if remove {
        parent.verify(root)?;
        same(
            &before,
            &rfs::statat(parent.file, name, AtFlags::SYMLINK_NOFOLLOW)?,
        )?;
        rfs::unlinkat(
            parent.file,
            name,
            if directory {
                AtFlags::REMOVEDIR
            } else {
                AtFlags::empty()
            },
        )?;
        rfs::fsync(parent.file)?;
    }
    Ok(())
}
