//! Unix: the effective uid, from `rustix` (no `unsafe` of ours), compared with the file's owner.

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The user as its effective uid.
#[derive(Debug)]
pub struct Id(u32);

impl super::User for Id {
    fn current() -> Id {
        Id(rustix::process::geteuid().as_raw())
    }

    // A symlink is judged itself (`symlink_metadata`).
    fn owns_path(&self, path: &Path) -> io::Result<bool> {
        Ok(std::fs::symlink_metadata(path)?.uid() == self.0)
    }
}
