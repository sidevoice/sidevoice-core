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

/// An endpoint as its socket path.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint(PathBuf);

impl super::Channel for Endpoint {
    fn in_dir(dir: &Path, name: &str) -> Endpoint {
        Endpoint(dir.join(format!("{name}.sock")))
    }

    fn from_record(record: &str) -> io::Result<Endpoint> {
        let path = PathBuf::from(record);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an absolute socket path: {record:?}"),
            ));
        }
        Ok(Endpoint(path))
    }

    fn socket_path(&self) -> Option<&Path> {
        Some(&self.0)
    }

    fn listen(&self) -> io::Result<Listener> {
        // A socket left by a dead process is replaced, once it is known to be ours and private and nobody answers
        // on it; a live one is somebody's name.
        if std::fs::symlink_metadata(&self.0).is_ok() {
            verify_socket(&self.0)?;
            if <interprocess::local_socket::Stream as SyncStream>::connect(self.name()?).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AddrInUse));
            }
            std::fs::remove_file(&self.0)?;
        }
        let listener = ListenerOptions::new()
            .name(self.name()?)
            .mode(0o600)
            .create_tokio()?;
        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(listener))
    }

    fn to_connect(&self) -> io::Result<Name<'static>> {
        verify_socket(&self.0)?;
        self.name()
    }

    fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
        Ok(stream.peer_creds()?.euid() == Some(rustix::process::geteuid().as_raw()))
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl Endpoint {
    fn name(&self) -> io::Result<Name<'static>> {
        self.0.clone().to_fs_name::<GenericFilePath>()
    }
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
    use super::super::Channel as _;
    use super::*;

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[tokio::test]
    async fn a_socket_is_trusted_only_in_a_private_directory_and_open_to_nobody_else() {
        let scratch = super::super::tests::scratch();
        let endpoint = Endpoint::in_dir(scratch.path(), "s");
        let _listener = endpoint.listen().unwrap();
        endpoint.connect().await.unwrap();
        chmod(&endpoint.0, 0o666);
        assert!(endpoint.connect().await.is_err());
        chmod(&endpoint.0, 0o600);
        chmod(scratch.path(), 0o755);
        assert!(endpoint.connect().await.is_err());
        chmod(scratch.path(), 0o700);
        let file = Endpoint::in_dir(scratch.path(), "plain");
        std::fs::write(&file.0, b"").unwrap();
        chmod(&file.0, 0o600);
        assert!(file.connect().await.is_err());
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_but_a_file_that_is_not_one_is_not() {
        let scratch = super::super::tests::scratch();
        let endpoint = Endpoint::in_dir(scratch.path(), "t");
        // Left behind by a process that died: the file stays, nothing listens.
        drop(std::os::unix::net::UnixListener::bind(&endpoint.0).unwrap());
        chmod(&endpoint.0, 0o600);
        assert!(endpoint.0.exists());
        let listener = endpoint.listen().unwrap();
        endpoint.connect().await.unwrap();
        drop(listener);
        assert!(
            !endpoint.0.exists(),
            "the socket file goes with its listener"
        );
        // Something that is not a socket is never unlinked.
        std::fs::write(&endpoint.0, b"mine").unwrap();
        chmod(&endpoint.0, 0o600);
        assert!(endpoint.listen().is_err());
        assert_eq!(std::fs::read(&endpoint.0).unwrap(), b"mine");
    }
}
