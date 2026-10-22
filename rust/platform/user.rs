//! This process's user: who it is, whether a path is theirs, what the host is called.
//!
//! The interface is the [`User`] trait; [`UserId`] is the one OS implementation compiled in, chosen below. Unix:
//! the effective uid, from `rustix`, compared with the file's owner. Windows: the user's SID, read from the process
//! token, compared (`EqualSid`) with the owner of the file's security descriptor. Callers import both:
//! `use crate::platform::user::{User, UserId};` then `UserId::current().owns_path(..)`, `UserId::host_name()`.
//!
//! The same trait as the connector's `platform::user`, less `owns_file`, which nothing in the core uses.

#[cfg(unix)]
#[path = "user/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "user/windows.rs"]
mod imp;

pub use imp::Id as UserId;

use std::io;
use std::path::Path;

/// What every OS implements ([`UserId`], in `user/unix.rs` and `user/windows.rs`).
pub trait User: Sized {
    /// The user this process runs as (Unix: the effective uid; Windows: the SID in its token).
    fn current() -> Self;
    /// Whether this user owns what `path` names. A symlink (or Windows junction) is judged itself, not what it
    /// points to.
    fn owns_path(&self, path: &Path) -> io::Result<bool>;
    /// The login name, for what must name the user to the system (the local channel's pipe name); `None` when the
    /// system cannot tell. One body for every OS (`whoami`); an OS overrides it only if it must.
    fn name() -> Option<String> {
        whoami::username().ok()
    }
    /// The host name as the operating system reports it, trimmed; empty when it cannot be read. One body for every OS
    /// (`gethostname`).
    fn host_name() -> String {
        gethostname::gethostname()
            .to_string_lossy()
            .trim()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_what_it_creates() {
        let dir = tempfile::tempdir().unwrap();
        assert!(UserId::current().owns_path(dir.path()).unwrap());
        let file = dir.path().join("f");
        std::fs::write(&file, b"").unwrap();
        assert!(UserId::current().owns_path(&file).unwrap());
    }

    #[test]
    fn owns_path_reports_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(UserId::current()
            .owns_path(&dir.path().join("missing"))
            .is_err());
    }

    #[test]
    fn name_is_known() {
        assert!(UserId::name().is_some_and(|name| !name.is_empty()));
    }

    #[test]
    fn host_name_is_trimmed() {
        let name = UserId::host_name();
        assert_eq!(name, name.trim());
    }
}
