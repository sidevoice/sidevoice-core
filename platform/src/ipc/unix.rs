//! Unix: a socket file in a private directory. The peer is checked by the kernel's credentials of the connection
//! (`SO_PEERCRED` / `LOCAL_PEERCRED`, through `peer_creds()`); a listener's socket is made, or one left behind
//! replaced, only in a directory of this user's open to nobody else.

use super::{Listener, Stream};
use crate::user::{User, UserId};
use interprocess::local_socket::traits::Stream as SyncStream;
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use std::fmt;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// An endpoint: its socket path.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint(PathBuf);

impl super::Address for Endpoint {
    fn new(dir: &Path, name: &str) -> Endpoint {
        let dir = super::endpoint_dir(<Os as super::Channel>::runtime_dir().as_deref(), dir);
        Endpoint(dir.join(format!("{name}.sock")))
    }
}

// A record is an absolute socket path.
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

impl Endpoint {
    /// The endpoint as `interprocess` names it.
    pub(super) fn name(&self) -> io::Result<Name<'static>> {
        self.0.clone().to_fs_name::<GenericFilePath>()
    }
}

/// Unix side of [`super::Channel`].
pub struct Os;

impl super::Channel for Os {
    // The per-user temporary directory, which `dirs` does not give.
    #[cfg(target_os = "macos")]
    fn runtime_dir() -> Option<PathBuf> {
        Some(std::env::temp_dir())
    }

    fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        let path = endpoint.0.as_path();
        // The endpoint's own directory in the runtime directory is made here; whatever is already there is judged
        // below like the directory it made.
        if let Some(parent) = path.parent() {
            match std::fs::DirBuilder::new().mode(0o700).create(parent) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                _ => {}
            }
        }
        // Only in a private directory: nobody else can reach the socket between its creation and its `chmod`
        // below. (Choosing the mode before `bind`, `interprocess`'s `mode`, is `Unsupported` on macOS, whose
        // `fchmod` refuses sockets.)
        verify_dir(path)?;
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
            .create_tokio()?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(listener))
    }
}

/// Whether the process at the other end of `stream` is this user: the connection's own credentials, the peer's
/// effective uid (`SO_PEERCRED` / `LOCAL_PEERCRED`), which the kernel vouches for (macOS gives no peer pid).
pub(super) fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    Ok(stream.peer_creds()?.euid() == Some(rustix::process::geteuid().as_raw()))
}

/// A socket before anything talks to it or replaces it: in a directory of this user's open to nobody else
/// ([`verify_dir`]), a socket, this user's, open to nobody else.
fn verify_socket(path: &Path) -> io::Result<()> {
    verify_dir(path)?;
    if !std::fs::symlink_metadata(path)?.file_type().is_socket() || !private(path)? {
        return Err(unsafe_socket(path));
    }
    Ok(())
}

/// The directory the socket `path` is in: a real directory (not a link), this user's, open to nobody else.
fn verify_dir(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| unsafe_socket(path))?;
    if !std::fs::symlink_metadata(parent)?.is_dir() || !private(parent)? {
        return Err(unsafe_socket(path));
    }
    Ok(())
}

/// This user's and open to nobody else, judged on `path` itself (no link followed).
fn private(path: &Path) -> io::Result<bool> {
    Ok(UserId::current().owns_path(path)? && std::fs::symlink_metadata(path)?.mode() & 0o077 == 0)
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
        &endpoint.0
    }

    /// The endpoint `name` in exactly `dir`, wherever this host's runtime directory is.
    fn in_dir(dir: &Path, name: &str) -> Endpoint {
        Endpoint(dir.join(format!("{name}.sock")))
    }

    /// Waits until nothing answers on the socket file `path`: the state a dead process leaves. Dropping a listener
    /// does not get there at once: a process spawned by a parallel test holds a copy of the descriptor from its
    /// `fork` until its `exec` closes it (the descriptors are close-on-exec), the listener stays alive meanwhile,
    /// and connecting succeeds. That is the test's own setup, not a socket whose owner is dead.
    fn await_nobody_answers(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::os::unix::net::UnixStream::connect(path).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "something still answers on {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[tokio::test]
    async fn a_listener_makes_its_missing_directory_private() {
        let scratch = super::super::tests::scratch();
        let dir = scratch.path().join("sidevoice-0123456789abcdef");
        let endpoint = in_dir(&dir, "m");
        let _listener = Channels::listen(&endpoint).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&dir).unwrap().mode() & 0o777,
            0o700
        );
        Channels::connect(&endpoint).await.unwrap();
    }

    #[test]
    fn a_socket_left_behind_open_to_others_is_not_replaced() {
        let scratch = super::super::tests::scratch();
        let endpoint = in_dir(scratch.path(), "s");
        drop(std::os::unix::net::UnixListener::bind(path(&endpoint)).unwrap());
        chmod(path(&endpoint), 0o666);
        assert!(Channels::listen(&endpoint).is_err());
        assert!(path(&endpoint).exists());
    }

    #[tokio::test]
    async fn a_listener_is_made_only_in_a_private_directory_and_its_socket_is_private() {
        let scratch = super::super::tests::scratch();
        let endpoint = in_dir(scratch.path(), "l");
        chmod(scratch.path(), 0o755);
        assert!(Channels::listen(&endpoint).is_err());
        assert!(!path(&endpoint).exists());
        chmod(scratch.path(), 0o700);
        let _listener = Channels::listen(&endpoint).unwrap();
        let mode = std::fs::symlink_metadata(path(&endpoint)).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced_but_a_file_that_is_not_one_is_not() {
        let scratch = super::super::tests::scratch();
        let endpoint = in_dir(scratch.path(), "t");
        // Left behind by a process that died: the file stays, nothing listens.
        drop(std::os::unix::net::UnixListener::bind(path(&endpoint)).unwrap());
        chmod(path(&endpoint), 0o600);
        assert!(path(&endpoint).exists());
        await_nobody_answers(path(&endpoint));
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
