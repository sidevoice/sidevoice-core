//! Unix: a socket file in a private directory. The peer is checked by the kernel's credentials of the connection
//! (`SO_PEERCRED` / `LOCAL_PEERCRED`, through `peer_creds()`); a socket is trusted before it is touched only when it
//! is a socket, this user's, and open to nobody else, in a directory open to nobody else.

use super::{Endpoint, Listener, Stream};
use crate::user::{User, UserId};
use interprocess::local_socket::traits::Stream as SyncStream;
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{ListenerOptions, Name};
use interprocess::os::unix::local_socket::ListenerOptionsExt;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

/// Unix side of [`super::Channel`].
pub struct Os;

impl super::Channel for Os {
    // The socket `dir/<name>.sock`.
    fn address(dir: &Path, name: &str) -> OsString {
        dir.join(format!("{name}.sock")).into_os_string()
    }

    // An absolute socket path.
    fn check(record: &str) -> io::Result<()> {
        if !Path::new(record).is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an absolute socket path: {record:?}"),
            ));
        }
        Ok(())
    }

    fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        let path = Path::new(&endpoint.0);
        // A socket left by a dead process is replaced, once it is known to be ours and private and nobody answers
        // on it; a live one is somebody's name.
        if std::fs::symlink_metadata(path).is_ok() {
            verify_socket(path)?;
            if <interprocess::local_socket::Stream as SyncStream>::connect(endpoint.name()?).is_ok()
            {
                return Err(io::Error::from(io::ErrorKind::AddrInUse));
            }
            std::fs::remove_file(path)?;
        }
        let listener = ListenerOptions::new()
            .name(endpoint.name()?)
            .mode(0o600)
            .create_tokio()?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(listener))
    }
}

/// What [`super::Channel::connect`] dials: the socket, once it is this user's, open to nobody else, in a private
/// directory.
pub(super) fn to_connect(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    verify_socket(Path::new(&endpoint.0))?;
    endpoint.name()
}

/// Whether the process at the other end of `stream` is this user: the connection's own credentials, the peer's
/// effective uid (`SO_PEERCRED` / `LOCAL_PEERCRED`), which the kernel vouches for (macOS gives no peer pid).
pub(super) fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    Ok(stream.peer_creds()?.euid() == Some(rustix::process::geteuid().as_raw()))
}

/// [`Endpoint::socket_path`]: the socket file the endpoint names.
pub(super) fn socket_path(endpoint: &Endpoint) -> Option<&Path> {
    Some(Path::new(&endpoint.0))
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

    fn path(endpoint: &Endpoint) -> &Path {
        endpoint.socket_path().unwrap()
    }

    #[tokio::test]
    async fn a_socket_is_trusted_only_in_a_private_directory_and_open_to_nobody_else() {
        let scratch = super::super::tests::scratch();
        let endpoint = Endpoint::new(scratch.path(), "s");
        let _listener = Channels::listen(&endpoint).unwrap();
        Channels::connect(&endpoint).await.unwrap();
        chmod(path(&endpoint), 0o666);
        assert!(Channels::connect(&endpoint).await.is_err());
        chmod(path(&endpoint), 0o600);
        chmod(scratch.path(), 0o755);
        assert!(Channels::connect(&endpoint).await.is_err());
        chmod(scratch.path(), 0o700);
        let file = Endpoint::new(scratch.path(), "plain");
        std::fs::write(path(&file), b"").unwrap();
        chmod(path(&file), 0o600);
        assert!(Channels::connect(&file).await.is_err());
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_but_a_file_that_is_not_one_is_not() {
        let scratch = super::super::tests::scratch();
        let endpoint = Endpoint::new(scratch.path(), "t");
        // Left behind by a process that died: the file stays, nothing listens.
        drop(std::os::unix::net::UnixListener::bind(path(&endpoint)).unwrap());
        chmod(path(&endpoint), 0o600);
        assert!(path(&endpoint).exists());
        let listener = Channels::listen(&endpoint).unwrap();
        Channels::connect(&endpoint).await.unwrap();
        drop(listener);
        assert!(
            !path(&endpoint).exists(),
            "the socket file goes with its listener"
        );
        // Something that is not a socket is never unlinked.
        std::fs::write(path(&endpoint), b"mine").unwrap();
        chmod(path(&endpoint), 0o600);
        assert!(Channels::listen(&endpoint).is_err());
        assert_eq!(std::fs::read(path(&endpoint)).unwrap(), b"mine");
    }
}
