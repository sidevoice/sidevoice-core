//! Unix: a socket file in a private directory. The peer is checked by the kernel's credentials of the connection
//! (`SO_PEERCRED` / `LOCAL_PEERCRED`, through `peer_creds()`); a socket is trusted before it is touched only when it
//! is a socket, this user's, and open to nobody else, in a directory open to nobody else.

use super::{Listener, Stream};
use crate::user::{User, UserId};
use interprocess::local_socket::traits::Stream as SyncStream;
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use interprocess::os::unix::local_socket::ListenerOptionsExt;
use std::fmt;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// An endpoint: its socket path.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint(PathBuf);

impl Endpoint {
    /// Our own endpoint called `name` for the data directory `dir`: the socket `dir/<name>.sock`.
    pub fn new(dir: &Path, name: &str) -> Endpoint {
        Endpoint(dir.join(format!("{name}.sock")))
    }

    /// The socket file. Only for what a file has and an address does not: the core's ready file keeps it as its
    /// deprecated `socket` field, and the core holds its directory besides its data directory.
    pub fn socket_path(&self) -> Option<&Path> {
        Some(&self.0)
    }

    fn name(&self) -> io::Result<Name<'static>> {
        self.0.clone().to_fs_name::<GenericFilePath>()
    }
}

/// A record is an absolute socket path; anything else is refused.
impl FromStr for Endpoint {
    type Err = io::Error;

    fn from_str(record: &str) -> io::Result<Endpoint> {
        let path = PathBuf::from(record);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an absolute socket path: {record:?}"),
            ));
        }
        Ok(Endpoint(path))
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

/// Unix side of [`super::Channel`].
pub struct Os;

impl super::Channel for Os {
    fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        // A socket left by a dead process is replaced, once it is known to be ours and private and nobody answers
        // on it; a live one is somebody's name.
        if std::fs::symlink_metadata(&endpoint.0).is_ok() {
            verify_socket(&endpoint.0)?;
            if <interprocess::local_socket::Stream as SyncStream>::connect(endpoint.name()?).is_ok()
            {
                return Err(io::Error::from(io::ErrorKind::AddrInUse));
            }
            std::fs::remove_file(&endpoint.0)?;
        }
        let listener = ListenerOptions::new()
            .name(endpoint.name()?)
            .mode(0o600)
            .create_tokio()?;
        std::fs::set_permissions(&endpoint.0, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(listener))
    }
}

/// What [`super::Channel::connect`] dials: the socket, once it is this user's, open to nobody else, in a private
/// directory.
pub(super) fn to_connect(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    verify_socket(&endpoint.0)?;
    endpoint.name()
}

/// Whether the process at the other end of `stream` is this user: the connection's own credentials, the peer's
/// effective uid (`SO_PEERCRED` / `LOCAL_PEERCRED`), which the kernel vouches for (macOS gives no peer pid).
pub(super) fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    Ok(stream.peer_creds()?.euid() == Some(rustix::process::geteuid().as_raw()))
}

/// A socket before anything talks to it or replaces it: in a directory of this user's open to nobody else, a
/// socket, this user's, open to nobody else.
fn verify_socket(path: &Path) -> io::Result<()> {
    let me = UserId::current();
    let private = |path: &Path| -> io::Result<bool> {
        Ok(me.owns_path(path)? && std::fs::symlink_metadata(path)?.mode() & 0o077 == 0)
    };
    let parent = path.parent().ok_or_else(|| unsafe_socket(path))?;
    if !std::fs::symlink_metadata(parent)?.is_dir()
        || !private(parent)?
        || !std::fs::symlink_metadata(path)?.file_type().is_socket()
        || !private(path)?
    {
        return Err(unsafe_socket(path));
    }
    Ok(())
}

fn unsafe_socket(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unsafe socket {}", path.display()),
    )
}

// Unix only: file modes and the socket file a dead process leaves behind have no counterpart on Windows, where a
// pipe has no file, no mode, and goes with its last handle.
#[cfg(test)]
mod tests {
    use super::super::{Channel as _, Channels};
    use super::*;

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[tokio::test]
    async fn a_socket_is_trusted_only_in_a_private_directory_and_open_to_nobody_else() {
        let scratch = super::super::tests::scratch();
        let endpoint = Endpoint::new(scratch.path(), "s");
        let _listener = Channels::listen(&endpoint).unwrap();
        Channels::connect(&endpoint).await.unwrap();
        chmod(&endpoint.0, 0o666);
        assert!(Channels::connect(&endpoint).await.is_err());
        chmod(&endpoint.0, 0o600);
        chmod(scratch.path(), 0o755);
        assert!(Channels::connect(&endpoint).await.is_err());
        chmod(scratch.path(), 0o700);
        let file = Endpoint::new(scratch.path(), "plain");
        std::fs::write(&file.0, b"").unwrap();
        chmod(&file.0, 0o600);
        assert!(Channels::connect(&file).await.is_err());
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_but_a_file_that_is_not_one_is_not() {
        let scratch = super::super::tests::scratch();
        let endpoint = Endpoint::new(scratch.path(), "t");
        // Left behind by a process that died: the file stays, nothing listens.
        drop(std::os::unix::net::UnixListener::bind(&endpoint.0).unwrap());
        chmod(&endpoint.0, 0o600);
        assert!(endpoint.0.exists());
        let listener = Channels::listen(&endpoint).unwrap();
        Channels::connect(&endpoint).await.unwrap();
        drop(listener);
        assert!(
            !endpoint.0.exists(),
            "the socket file goes with its listener"
        );
        // Something that is not a socket is never unlinked.
        std::fs::write(&endpoint.0, b"mine").unwrap();
        chmod(&endpoint.0, 0o600);
        assert!(Channels::listen(&endpoint).is_err());
        assert_eq!(std::fs::read(&endpoint.0).unwrap(), b"mine");
    }
}
