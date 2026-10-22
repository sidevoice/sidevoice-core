//! Unix: the effective uid, from `rustix` (no `unsafe` of ours).

// The raw calls the rest of the crate is not allowed to make (`clippy.toml`) are made here.
#![allow(clippy::disallowed_methods)]

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

/// Unix side of [`super::UserId`]: the effective uid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Id(u32);

pub(super) fn current() -> Id {
    Id(rustix::process::geteuid().as_raw())
}

impl Id {
    pub(super) fn owns(&self, metadata: &Metadata) -> bool {
        metadata.uid() == self.0
    }

    pub(super) fn raw(&self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_what_it_creates() {
        let dir =
            std::env::temp_dir().join(format!("sv-platform-user-unix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(current().owns(&std::fs::metadata(&dir).unwrap()));
        assert_eq!(current().raw(), rustix::process::geteuid().as_raw());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
