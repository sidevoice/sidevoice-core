//! This process's user. Unix: the effective uid, from `rustix`. Windows: the user's SID, read from a process token.

#[cfg(unix)]
#[path = "user/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "user/windows.rs"]
mod imp;

// The whole interface is here, documented once; `imp` only implements it per OS, so a side that does not provide
// an operation does not compile.

/// The user this process runs as. Opaque: callers ask it questions and never see what it is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserId(imp::Id);

/// The user this process runs as (Unix: the effective uid; Windows: the user SID in its token).
pub fn current() -> UserId {
    UserId(imp::current())
}

impl UserId {
    /// Whether this user owns the file `metadata` describes. Unix-only: `Metadata` carries no owner on Windows.
    #[cfg(unix)]
    pub fn owns(&self, metadata: &std::fs::Metadata) -> bool {
        self.0.owns(metadata)
    }

    /// The numeric uid. Unix-only; nothing host-agnostic needs it.
    #[cfg(unix)]
    pub fn raw(&self) -> u32 {
        self.0.raw()
    }

    /// The user the process `pid` runs as. Windows-only: the pipe's peer is known by process id there.
    #[cfg(windows)]
    pub fn of_process(pid: u32) -> std::io::Result<UserId> {
        imp::of_process(pid).map(UserId)
    }

    /// The user's SID in its string form (`S-1-5-...`), for security descriptors and names. Windows-only.
    #[cfg(windows)]
    pub fn sid_string(&self) -> std::io::Result<String> {
        self.0.sid_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_is_stable() {
        assert_eq!(current(), current());
    }
}
