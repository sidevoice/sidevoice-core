//! Windows: a named pipe, `\\.\pipe\sidevoice-<hash of (user name, data dir)>-<name>`. The pipe is created as the
//! first instance (a name already taken is an error) and refusing remote clients, both `interprocess`'s defaults,
//! with a protected descriptor that lets in only its owner (`OW`: the user whose process made it), SYSTEM and
//! Administrators. The peer check: the process at the other end of the pipe belongs to this user.

use super::{Endpoint, Listener, Stream};
use crate::user::{User, UserId};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{ListenerOptions, Name};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io;
use std::path::Path;

const PREFIX: &str = r"\\.\pipe\";

/// Full access for the pipe's owner, SYSTEM and Administrators; nobody else, nothing inherited.
const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)";

/// Windows side of [`super::Channel`].
pub struct Os;

impl super::Channel for Os {
    // The pipe `\\.\pipe\sidevoice-<hash of (user, dir)>-<name>`: two profiles or two users never meet on one name.
    fn address(dir: &Path, name: &str) -> OsString {
        let mut hash = Sha256::new();
        hash.update(UserId::name().unwrap_or_default().to_lowercase().as_bytes());
        hash.update([0]);
        hash.update(dir.to_string_lossy().to_lowercase().as_bytes());
        format!(
            "{PREFIX}sidevoice-{}-{name}",
            hex::encode(&hash.finalize()[..8])
        )
        .into()
    }

    // A full local pipe name, `\\.\pipe\...`.
    fn check(record: &str) -> io::Result<()> {
        if record.len() <= PREFIX.len() || !record.starts_with(PREFIX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a local pipe name: {record:?}"),
            ));
        }
        Ok(())
    }

    fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        let descriptor = widestring::U16CString::from_str(OWNER_ONLY).map_err(io::Error::other)?;
        ListenerOptions::new()
            .name(endpoint.name()?)
            .security_descriptor(SecurityDescriptor::deserialize(&descriptor)?)
            .create_tokio()
            .map(Listener)
    }
}

/// What [`super::Channel::connect`] dials: the pipe by its name; the server is checked once connected.
pub(super) fn to_connect(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    endpoint.name()
}

/// Whether the process at the other end of `stream` is this user's: the pipe's client or server pid, as
/// `interprocess` reports it, is a live process of this user's.
pub(super) fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    let pid = stream
        .peer_creds()?
        .pid()
        .ok_or_else(|| io::Error::other("the peer process is unknown"))?;
    Ok(is_this_users_process(pid))
}

/// [`Endpoint::socket_path`]: a pipe has no file.
pub(super) fn socket_path(_: &Endpoint) -> Option<&Path> {
    None
}

/// Whether `pid` is, now, a live process (not a zombie) of the user this one runs as (`sysinfo`).
fn is_this_users_process(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};
    let owner = |pid: Pid| {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_user(UpdateKind::Always),
        );
        let process = system.process(pid)?;
        (process.status() != ProcessStatus::Zombie)
            .then(|| process.user_id().cloned())
            .flatten()
    };
    // 0 and 1 are never a peer (the idle process, and init in the Unix sense).
    if pid <= 1 {
        return false;
    }
    let Ok(this) = sysinfo::get_current_pid() else {
        return false;
    };
    matches!((owner(Pid::from_u32(pid)), owner(this)), (Some(peer), Some(me)) if peer == me)
}

// Windows only: the pid lookup is the Windows peer check's own helper (Unix asks the kernel for the peer's uid), and
// the generic tests only reach it with this process as the peer; here, the pids that must never pass.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_this_users_and_nothing_else_is() {
        assert!(is_this_users_process(std::process::id()));
        assert!(!is_this_users_process(0));
        assert!(!is_this_users_process(1));
        assert!(!is_this_users_process(u32::MAX));
    }
}
