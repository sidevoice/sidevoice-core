//! The local channel between this user's processes: the core's endpoint (where the connector, the desktop app and
//! the core's own readiness probe reach it) and the connector's endpoint for its clients.
//!
//! Two traits, each with the one OS implementation compiled in, chosen below: a Unix socket on macOS and Linux, a
//! named pipe on Windows, both through `interprocess`. [`Address`] is where a channel is, implemented by
//! [`Endpoint`]: a process makes its own with `Endpoint::new(dir, name)` and publishes its `Display`, the record
//! (the core's ready file, the `--endpoint` flag), which whoever reads it parses back (`record.parse::<Endpoint>()`).
//! [`Channel`], implemented by [`Channels`], listens and connects. What this module adds is the user check, on every
//! OS and on both ends: a listener only ever hands out streams whose peer is this user, and a client only talks to a
//! server that is this user. Streams are plain `AsyncRead + AsyncWrite + Unpin`, so what runs over them (an HTTP API
//! and its WebSockets, a line-JSON protocol) runs unchanged. Callers import the traits and the types, std-style
//! (`TcpListener::bind(addr)`): `use sidevoice_platform::ipc::{Address, Channel, Channels, Endpoint};` then
//! `Channels::listen(&Endpoint::new(dir, "local"))`, `Channels::connect(&endpoint).await`. Every crate computes the
//! same pipe for the same directory and name, because they all run this one implementation.
//!
//! Connecting is bounded where the OS waits, not only where the caller does ([`Channel::connect_within`], and
//! [`CONNECT_WAIT`] for [`Channel::connect`]). A Windows pipe whose every instance is taken (a stalled server, or
//! another local user who created the name first and holds it) answers `ERROR_PIPE_BUSY`, and waiting for a free
//! instance is a blocking `WaitNamedPipe` on one of Tokio's blocking threads. That wait gets the connect's own budget:
//! the thread is back once the budget is spent, whether or not anyone still awaits it, so repeated attempts against a
//! busy pipe never pile up parked threads. A Unix socket never waits for a busy server: a full backlog fails at once.

#[cfg(unix)]
#[path = "ipc/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "ipc/windows.rs"]
mod imp;

pub use imp::{Endpoint, Os as Channels};
// The pipe's descriptor, which a process's stop event shares (`crate::stop`).
#[cfg(windows)]
pub(crate) use imp::OWNER_ONLY;

use interprocess::local_socket::tokio::Listener as RawListener;
use interprocess::local_socket::traits::tokio::Listener as _;
use sha2::{Digest, Sha256};
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// How long [`Channel::connect`] waits, at most, for a server to take the connection (on Windows, for a free instance
/// of a busy pipe). A server that is up takes it at once; this only bounds a stalled or squatted one.
pub const CONNECT_WAIT: Duration = Duration::from_secs(5);

/// One connection. `AsyncRead + AsyncWrite + Unpin`; its peer has been checked to be this user.
pub type Stream = interprocess::local_socket::tokio::Stream;

/// Where a channel is, as every OS implements it ([`Endpoint`], in `ipc/unix.rs` and `ipc/windows.rs`): an opaque
/// address. `Display` writes it as its record (the core's ready file, the `--endpoint` flag) and `FromStr` reads one
/// back, refusing anything this OS cannot reach (`InvalidInput`). Nothing derives one from another or looks inside.
pub trait Address:
    Sized + Clone + fmt::Debug + fmt::Display + PartialEq + FromStr<Err = io::Error>
{
    /// Our own endpoint called `name` for the data directory `dir`. Unix: the socket `<name>.sock` in this user's
    /// runtime directory ([`Channel::runtime_dir`]), inside a directory of `dir`'s own, `sidevoice-<hash of dir>`, so
    /// the path stays short whatever `dir` is and two data directories never share one; without a runtime
    /// directory, `dir/<name>.sock`. Windows: the pipe `\\.\pipe\sidevoice-<hash of (user, dir)>-<name>`, so two
    /// profiles or two users never meet on one name. A record read back is, on Unix, an absolute socket path, and on
    /// Windows a full local pipe name.
    fn new(dir: &Path, name: &str) -> Self;
}

/// The directory an endpoint for the data directory `dir` is named in, given this user's runtime directory
/// ([`Channel::runtime_dir`]): `runtime/sidevoice-<first 8 bytes of SHA-256(dir), in hex>`, or `dir` itself without
/// one.
fn endpoint_dir(runtime: Option<&Path>, dir: &Path) -> PathBuf {
    match runtime {
        Some(runtime) => {
            let hash = Sha256::digest(dir.as_os_str().as_encoded_bytes());
            runtime.join(format!("sidevoice-{}", hex::encode(&hash[..8])))
        }
        None => dir.to_owned(),
    }
}

/// What every OS implements ([`Channels`], in `ipc/unix.rs` and `ipc/windows.rs`): how a listener is made private to
/// this user. Connecting is one body for every OS around each OS file's bounded connect (`connect`); the peer check is
/// internal to each OS file (`peer_is_this_user`).
pub trait Channel {
    /// This user's runtime directory, where [`Address::new`] puts the endpoints' directories so a socket path stays
    /// within the OS limit (104 bytes on macOS, 108 on Linux) however long the data directory is. Linux:
    /// `$XDG_RUNTIME_DIR` (`/run/user/<uid>`). macOS: the per-user temporary directory, `$TMPDIR`, else
    /// `confstr(_CS_DARWIN_USER_TEMP_DIR)` (`/var/folders/…/T/`), as `std::env::temp_dir` reads it. `None` where
    /// there is none (Linux without a login session, a container) and on Windows, whose pipes have no path: the
    /// endpoints then stay in their data directory. One body for every OS (`dirs`); macOS overrides it.
    fn runtime_dir() -> Option<PathBuf> {
        dirs::runtime_dir()
    }

    /// Starts listening at `endpoint`, reachable by this user alone. A name another process listens on is an error.
    /// Unix: only in a directory of this user's open to nobody else, made (`0700`) when missing, one already there
    /// used only if it passes (refused, never repaired); a socket left by a dead process is replaced,
    /// but only once it is checked to be this user's and private; the socket file is `0600` and goes away when the
    /// listener is dropped. Windows: the first instance of the pipe, with a descriptor that lets in its owner,
    /// SYSTEM and Administrators only, refusing remote clients.
    fn listen(endpoint: &Endpoint) -> io::Result<Listener>;

    /// Connects to `endpoint`, refusing a server that is not this user: what is checked is the process at the other
    /// end, once connected, not the file or pipe before. Gives up after [`CONNECT_WAIT`]
    /// ([`connect_within`](Channel::connect_within)).
    fn connect(endpoint: &Endpoint) -> impl Future<Output = io::Result<Stream>> + Send {
        Self::connect_within(endpoint, CONNECT_WAIT)
    }

    /// [`connect`](Channel::connect), giving up with `TimedOut` once `wait` has passed: one deadline for the whole
    /// connect, retries included. The OS's own wait gets the same bound: on Windows, waiting for a free instance of a
    /// busy pipe (`WaitNamedPipe`, on a Tokio blocking thread) ends within `wait`, so that thread is back by then even
    /// if this future was dropped before; with `Duration::ZERO` it never waits (a busy pipe is `TimedOut` at once).
    /// Unix never waits for a busy server. The peer check comes after, unchanged, and a check that has started is not
    /// cut short. Needs a Tokio runtime with its timer enabled.
    fn connect_within(
        endpoint: &Endpoint,
        wait: Duration,
    ) -> impl Future<Output = io::Result<Stream>> + Send {
        let connecting = imp::connect(endpoint, wait);
        let at = endpoint.to_string();
        async move {
            let stream = match tokio::time::timeout(wait, connecting).await {
                Ok(Err(error)) if error.kind() != io::ErrorKind::TimedOut => return Err(error),
                Ok(Ok(stream)) => stream,
                Ok(Err(_)) | Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("the server at {at} did not take the connection within {wait:?}"),
                    ))
                }
            };
            if !imp::peer_is_this_user(&stream)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("the server at {at} is not this user"),
                ));
            }
            Ok(stream)
        }
    }
}

/// A listening endpoint ([`Channel::listen`]).
pub struct Listener(RawListener);

impl Listener {
    /// The next connection whose peer is this user. Any other is closed without a word and not reported.
    pub async fn accept(&self) -> io::Result<Stream> {
        loop {
            let stream = self.0.accept().await?;
            if imp::peer_is_this_user(&stream).unwrap_or(false) {
                return Ok(stream);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A scratch directory open to this user alone, removed on drop.
    pub(super) fn scratch() -> tempfile::TempDir {
        let dir = tempfile::Builder::new().prefix("sv-ipc").tempdir().unwrap();
        // Unix trusts a socket only in a directory open to nobody else; a pipe has no directory.
        #[cfg(unix)]
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        dir
    }

    #[test]
    fn an_endpoint_survives_its_record_and_nothing_else_is_one() {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "test");
        assert_eq!(endpoint.to_string().parse::<Endpoint>().unwrap(), endpoint);
        assert!("".parse::<Endpoint>().is_err());
        assert!("relative/local.sock".parse::<Endpoint>().is_err());
    }

    #[test]
    fn a_directory_and_a_name_make_one_endpoint_and_nothing_else_does() {
        let (a, b) = (scratch(), scratch());
        let endpoint = Endpoint::new(a.path(), "local");
        assert_eq!(endpoint, Endpoint::new(a.path(), "local"));
        assert_ne!(endpoint, Endpoint::new(b.path(), "local"));
        assert_ne!(endpoint, Endpoint::new(a.path(), "connector"));
        assert!(endpoint.to_string().contains("local"));
    }

    /// A data directory a 22-byte user name and a profile make too long for a socket in it, and a macOS per-user
    /// temporary directory, as long as it gets.
    const LONG_DATA_DIR: &str =
        "/Users/a-twenty-two-byte-user/Library/Application Support/Sidevoice-profiles/a-long-profile-name/core";
    const MACOS_TMPDIR: &str = "/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/T/";
    /// The most a Unix socket path may hold on macOS (`sun_path`), its last byte the terminating NUL.
    const SUN_PATH: usize = 104;

    #[test]
    fn under_a_runtime_directory_a_long_data_directory_makes_a_short_endpoint_of_its_own() {
        let runtime = Path::new(MACOS_TMPDIR);
        let dir = endpoint_dir(Some(runtime), Path::new(LONG_DATA_DIR));
        assert!(dir.starts_with(runtime));
        assert!(dir.join("connector.sock").as_os_str().len() < SUN_PATH);
        assert!(
            Path::new(LONG_DATA_DIR)
                .join("connector.sock")
                .as_os_str()
                .len()
                >= SUN_PATH
        );
        assert_eq!(dir, endpoint_dir(Some(runtime), Path::new(LONG_DATA_DIR)));
        assert_ne!(
            dir,
            endpoint_dir(Some(runtime), &Path::new(LONG_DATA_DIR).join("other"))
        );
        assert_eq!(
            endpoint_dir(None, Path::new(LONG_DATA_DIR)),
            Path::new(LONG_DATA_DIR)
        );
    }

    #[test]
    fn with_a_runtime_directory_here_a_long_data_directory_still_makes_a_reachable_endpoint() {
        // Where this host has none, endpoints stay in their data directory (Unix) or have no path (Windows).
        if Channels::runtime_dir().is_none() {
            return;
        }
        let endpoint = Endpoint::new(Path::new(LONG_DATA_DIR), "connector");
        assert!(endpoint.to_string().len() < SUN_PATH, "{endpoint}");
    }

    #[tokio::test]
    async fn this_user_listens_and_connects_and_bytes_flow_both_ways() {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "test");
        let listener = Channels::listen(&endpoint).unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            assert!(imp::peer_is_this_user(&stream).unwrap());
            let mut line = [0u8; 5];
            stream.read_exact(&mut line).await.unwrap();
            stream.write_all(&line).await.unwrap();
        });
        let mut client = Channels::connect(&endpoint).await.unwrap();
        assert!(imp::peer_is_this_user(&client).unwrap());
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_second_listener_on_a_live_endpoint_is_refused() {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "twice");
        let _first = Channels::listen(&endpoint).unwrap();
        assert!(Channels::listen(&endpoint).is_err());
        Channels::connect(&endpoint).await.unwrap();
    }

    #[tokio::test]
    async fn a_bounded_connect_reaches_a_listener_that_is_up() {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "bounded");
        let _listener = Channels::listen(&endpoint).unwrap();
        Channels::connect_within(&endpoint, Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_endpoint_nobody_listens_at_is_an_error_not_a_hang() {
        let dir = scratch();
        assert!(Channels::connect(&Endpoint::new(dir.path(), "test"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_endpoint_is_free_again_once_its_listener_is_dropped() {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "test");
        drop(Channels::listen(&endpoint).unwrap());
        let _listener = Channels::listen(&endpoint).unwrap();
        Channels::connect(&endpoint).await.unwrap();
    }
}
