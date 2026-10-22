//! The local channel between this user's processes: the core's endpoint (where the connector, the desktop app and
//! the core's own readiness probe reach it) and the connector's endpoint for its clients.
//!
//! The interface is the [`Channel`] trait; [`Endpoint`] is the one OS implementation compiled in, chosen below: a
//! Unix socket on macOS and Linux, a named pipe on Windows, both through `interprocess`. What this module adds is
//! the user check, on every OS and on both ends: a listener only ever hands out streams whose peer is this user, and
//! a client only talks to a server that is this user. Streams are plain `AsyncRead + AsyncWrite + Unpin`, so what
//! runs over them (an HTTP API and its WebSockets, a line-JSON protocol) runs unchanged. Callers import both:
//! `use sidevoice_platform::ipc::{Channel, Endpoint};` then `Endpoint::in_dir(dir, "local").listen()`,
//! `endpoint.connect().await`.
//!
//! An endpoint is opaque: a process makes its own with [`Channel::in_dir`], and publishes it as its record, its
//! `Display` (the core's ready file, the `--endpoint` flag), which whoever reads it passes to
//! [`Channel::from_record`]; nothing derives one from the other or looks inside. Every crate computes the same pipe
//! for the same directory and name, because they all run this one implementation.

#[cfg(unix)]
#[path = "ipc/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "ipc/windows.rs"]
mod imp;

pub use imp::Endpoint;

use interprocess::local_socket::tokio::Listener as RawListener;
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::Name;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::Path;

/// One connection. `AsyncRead + AsyncWrite + Unpin`; its peer has been checked to be this user.
pub type Stream = interprocess::local_socket::tokio::Stream;

/// What every OS implements ([`Endpoint`], in `ipc/unix.rs` and `ipc/windows.rs`): where a channel is, and how a
/// listener is made private to this user. Connecting and the peer check have one body for every OS; an OS overrides
/// them only if it must.
pub trait Channel: Sized + Clone + fmt::Debug + fmt::Display + PartialEq + Send + Sync {
    /// Our own endpoint called `name` for the data directory `dir`. Unix: the socket `dir/<name>.sock`. Windows: the
    /// pipe `\\.\pipe\sidevoice-<hash of (user, dir)>-<name>`, so two profiles or two users never meet on one name.
    fn in_dir(dir: &Path, name: &str) -> Self;

    /// An endpoint published as its `Display` record (the ready file's `endpoint`, the `--endpoint` flag). Unix: an
    /// absolute socket path. Windows: a full pipe name, `\\.\pipe\...`. Anything else is refused.
    fn from_record(record: &str) -> io::Result<Self>;

    /// The socket file, where the channel is one (Unix); `None` where it has no file (a Windows pipe). The core's
    /// ready file keeps it as its deprecated `socket` field, and holds its directory besides its data directory.
    fn socket_path(&self) -> Option<&Path>;

    /// Starts listening, reachable by this user alone. A name another process listens on is an error. Unix: a socket
    /// left by a dead process is replaced, but only once it is checked to be this user's and private; the socket
    /// file is `0600` and goes away when the listener is dropped. Windows: the first instance of the pipe, with a
    /// descriptor that lets in its owner, SYSTEM and Administrators only, refusing remote clients.
    fn listen(&self) -> io::Result<Listener>;

    /// What [`Channel::connect`] connects to, once what must hold of the endpoint before talking to it holds. Unix:
    /// a socket of this user's, open to nobody else, in a private directory. Windows: nothing; the server is checked
    /// once connected.
    fn to_connect(&self) -> io::Result<Name<'static>>;

    /// Connects, refusing a server that is not this user ([`Channel::peer_is_this_user`]). No timeout: callers
    /// bound it.
    fn connect(&self) -> impl Future<Output = io::Result<Stream>> + Send {
        async move {
            let stream = Stream::connect(self.to_connect()?).await?;
            if !Self::peer_is_this_user(&stream)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("the server at {self} is not this user"),
                ));
            }
            Ok(stream)
        }
    }

    /// Whether the process at the other end of `stream` is this user's. One body: the peer's pid, as the OS reports
    /// it for the connection (Windows: the pipe's client or server process), is a live process of this user's
    /// (`sysinfo`). Unix overrides it with the connection's own credentials (the peer's effective uid,
    /// `SO_PEERCRED` / `LOCAL_PEERCRED`): the kernel's word, and macOS gives no peer pid.
    fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
        let pid = stream
            .peer_creds()?
            .pid()
            // `interprocess` gives the OS's pid type (u32 on Windows, i32 on Unix): widened, then narrowed.
            .and_then(|pid| u32::try_from(i64::from(pid)).ok())
            .ok_or_else(|| io::Error::other("the peer process is unknown"))?;
        Ok(is_this_users_process(pid))
    }
}

/// A listening endpoint ([`Channel::listen`]).
pub struct Listener(RawListener);

impl Listener {
    /// The next connection whose peer is this user. Any other is closed without a word and not reported.
    pub async fn accept(&self) -> io::Result<Stream> {
        loop {
            let stream = self.0.accept().await?;
            if Endpoint::peer_is_this_user(&stream).unwrap_or(false) {
                return Ok(stream);
            }
        }
    }
}

/// Whether `pid` is, now, a live process (not a zombie) of the user this one runs as.
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
    // 0 and 1 are never a peer (the idle or init process).
    if pid <= 1 {
        return false;
    }
    let Ok(this) = sysinfo::get_current_pid() else {
        return false;
    };
    matches!((owner(Pid::from_u32(pid)), owner(this)), (Some(peer), Some(me)) if peer == me)
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
        let endpoint = Endpoint::in_dir(dir.path(), "test");
        assert_eq!(
            Endpoint::from_record(&endpoint.to_string()).unwrap(),
            endpoint
        );
        assert!(Endpoint::from_record("").is_err());
        assert!(Endpoint::from_record("relative/local.sock").is_err());
    }

    #[test]
    fn a_directory_and_a_name_make_one_endpoint_and_nothing_else_does() {
        let (a, b) = (scratch(), scratch());
        let endpoint = Endpoint::in_dir(a.path(), "local");
        assert_eq!(endpoint, Endpoint::in_dir(a.path(), "local"));
        assert_ne!(endpoint, Endpoint::in_dir(b.path(), "local"));
        assert_ne!(endpoint, Endpoint::in_dir(a.path(), "connector"));
        assert!(endpoint.to_string().contains("local"));
    }

    #[test]
    fn a_socket_path_is_inside_the_directory_when_there_is_one() {
        let dir = scratch();
        let endpoint = Endpoint::in_dir(dir.path(), "local");
        if let Some(socket) = endpoint.socket_path() {
            assert_eq!(socket.parent(), Some(dir.path()));
            assert_eq!(socket.to_str(), Some(endpoint.to_string().as_str()));
        }
    }

    #[tokio::test]
    async fn this_user_listens_and_connects_and_bytes_flow_both_ways() {
        let dir = scratch();
        let endpoint = Endpoint::in_dir(dir.path(), "test");
        let listener = endpoint.listen().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            assert!(Endpoint::peer_is_this_user(&stream).unwrap());
            let mut line = [0u8; 5];
            stream.read_exact(&mut line).await.unwrap();
            stream.write_all(&line).await.unwrap();
        });
        let mut client = endpoint.connect().await.unwrap();
        assert!(Endpoint::peer_is_this_user(&client).unwrap());
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        server.await.unwrap();
    }

    #[test]
    fn this_process_is_this_users_and_nothing_else_is() {
        assert!(is_this_users_process(std::process::id()));
        assert!(!is_this_users_process(0));
        assert!(!is_this_users_process(1));
        assert!(!is_this_users_process(u32::MAX));
    }

    #[tokio::test]
    async fn a_second_listener_on_a_live_endpoint_is_refused() {
        let dir = scratch();
        let endpoint = Endpoint::in_dir(dir.path(), "twice");
        let _first = endpoint.listen().unwrap();
        assert!(endpoint.listen().is_err());
        endpoint.connect().await.unwrap();
    }

    #[tokio::test]
    async fn an_endpoint_nobody_listens_at_is_an_error_not_a_hang() {
        let dir = scratch();
        assert!(Endpoint::in_dir(dir.path(), "test")
            .connect()
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_endpoint_is_free_again_once_its_listener_is_dropped() {
        let dir = scratch();
        let endpoint = Endpoint::in_dir(dir.path(), "test");
        drop(endpoint.listen().unwrap());
        let _listener = endpoint.listen().unwrap();
        endpoint.connect().await.unwrap();
    }
}
