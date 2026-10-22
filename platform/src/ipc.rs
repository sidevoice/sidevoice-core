//! The local channel between this user's processes: the core's endpoint (where the connector, the desktop app and
//! the core's own readiness probe reach it) and the connector's endpoint for its clients.
//!
//! The interface is the [`Channel`] trait; [`Channels`] is the one OS implementation compiled in, chosen below: a
//! Unix socket on macOS and Linux, a named pipe on Windows, both through `interprocess`. [`Endpoint`] is only an
//! address, opaque: a process makes its own with `Endpoint::new(dir, name)` (Unix: the socket `dir/<name>.sock`;
//! Windows: the pipe `\\.\pipe\sidevoice-<hash of (user, dir)>-<name>`, so two profiles or two users never meet on
//! one name), and publishes its `Display`, the record (the core's ready file, the `--endpoint` flag), which whoever
//! reads it parses back (`record.parse::<Endpoint>()`; Unix: an absolute socket path; Windows: a full pipe name).
//! Nothing derives one from the other or looks inside. What this module adds is the user check, on every OS and on
//! both ends: a listener only ever hands out streams whose peer is this user, and a client only talks to a server
//! that is this user. Streams are plain `AsyncRead + AsyncWrite + Unpin`, so what runs over them (an HTTP API and
//! its WebSockets, a line-JSON protocol) runs unchanged. Callers import the trait and the types, std-style
//! (`TcpListener::bind(addr)`): `use sidevoice_platform::ipc::{Channel, Channels, Endpoint};` then
//! `Channels::listen(&Endpoint::new(dir, "local"))`, `Channels::connect(&endpoint).await`. Every crate computes the
//! same pipe for the same directory and name, because they all run this one implementation.

#[cfg(unix)]
#[path = "ipc/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "ipc/windows.rs"]
mod imp;

pub use imp::{Endpoint, Os as Channels};

use interprocess::local_socket::tokio::Listener as RawListener;
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use std::future::Future;
use std::io;

/// One connection. `AsyncRead + AsyncWrite + Unpin`; its peer has been checked to be this user.
pub type Stream = interprocess::local_socket::tokio::Stream;

/// What every OS implements ([`Channels`], in `ipc/unix.rs` and `ipc/windows.rs`): how a listener is made private to
/// this user. Connecting is one body for every OS. What is checked before connecting and the peer check are internal:
/// each OS file has its `to_connect` and `peer_is_this_user`.
pub trait Channel {
    /// Starts listening at `endpoint`, reachable by this user alone. A name another process listens on is an
    /// error. Unix: a socket left by a dead process is replaced, but only once it is checked to be this user's and
    /// private; the socket file is `0600` and goes away when the listener is dropped. Windows: the first instance of
    /// the pipe, with a descriptor that lets in its owner, SYSTEM and Administrators only, refusing remote clients.
    fn listen(endpoint: &Endpoint) -> io::Result<Listener>;

    /// Connects to `endpoint`, refusing a server that is not this user. Unix: first, the socket must be this
    /// user's, open to nobody else, in a private directory. No timeout: callers bound it.
    fn connect(endpoint: &Endpoint) -> impl Future<Output = io::Result<Stream>> + Send {
        let name = imp::to_connect(endpoint);
        let at = endpoint.to_string();
        async move {
            let stream = Stream::connect(name?).await?;
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

    #[test]
    fn a_socket_path_is_inside_the_directory_when_there_is_one() {
        let dir = scratch();
        let endpoint = Endpoint::new(dir.path(), "local");
        if let Some(socket) = endpoint.socket_path() {
            assert_eq!(socket.parent(), Some(dir.path()));
            assert_eq!(socket.to_str(), Some(endpoint.to_string().as_str()));
        }
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
