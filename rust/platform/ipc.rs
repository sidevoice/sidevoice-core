//! The local channel between this machine's processes: a Unix socket on macOS and Linux, a named pipe on Windows,
//! both through `interprocess`. Whoever is at the other end is checked to be this user, on every OS and on both
//! sides: a listener drops the connections of other users, and `connect` refuses a server that is not this user.

#[cfg(unix)]
#[path = "ipc/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "ipc/windows.rs"]
mod imp;

use std::io;
use std::path::Path;

use interprocess::local_socket::tokio::Listener as RawListener;
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};

use super::user::{self, UserId};

// The whole interface is here, documented once; `imp` only implements it per OS, so a side that does not provide
// an operation does not compile.

/// An accepted or connected connection: `AsyncRead + AsyncWrite`.
pub type Stream = interprocess::local_socket::tokio::Stream;

/// Where a core listens. Opaque, and written in the ready file as a string (the record) so that whoever reads it
/// connects without deriving anything: on Unix the socket path, on Windows the pipe name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint(imp::Endpoint);

impl Endpoint {
    /// The endpoint of the core whose data directory is `data_dir`. Unix: `local.sock` inside it. Windows:
    /// `\\.\pipe\sidevoice-<hash of this user's SID and the directory>-core`, so two users, or two data
    /// directories, never share a pipe.
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        imp::Endpoint::new(data_dir).map(Self)
    }

    /// The endpoint a record names (see [`Endpoint::to_record`]). Unix: a relative path is made absolute.
    pub fn from_record(record: &str) -> io::Result<Self> {
        imp::Endpoint::from_record(record).map(Self)
    }

    /// The endpoint at socket path `path`. Unix-only (the `--socket` flag).
    #[cfg(unix)]
    pub fn from_socket_path(path: &Path) -> io::Result<Self> {
        imp::Endpoint::from_path(path).map(Self)
    }

    /// The string that names this endpoint in the ready file.
    pub fn to_record(&self) -> String {
        self.0.to_record()
    }

    /// The socket file, where there is one (Unix; `None` on Windows). Its directory is what the core locks.
    pub fn socket_path(&self) -> Option<&Path> {
        self.0.socket_path()
    }
}

/// A listening endpoint. [`Listener::accept`] only yields connections whose peer is this user.
pub struct Listener {
    inner: RawListener,
    user: UserId,
    _guard: imp::Guard,
}

/// Start listening on `endpoint`. Fails with `AddrInUse` (or `PermissionDenied` on Windows) when another process
/// owns the name; Unix replaces a socket file nobody answers on, and removes its own file on drop.
pub async fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
    let (inner, guard) = imp::listen(&endpoint.0).await?;
    Ok(Listener {
        inner,
        user: user::current(),
        _guard: guard,
    })
}

impl Listener {
    /// The next connection from this user. Connections from anyone else are closed without being yielded.
    pub async fn accept(&self) -> io::Result<Stream> {
        loop {
            let stream = self.inner.accept().await?;
            if imp::peer_is(&stream, &self.user).unwrap_or(false) {
                return Ok(stream);
            }
        }
    }
}

/// Connect to the server at `endpoint`; `PermissionDenied` if it is not this user's.
pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    let stream = Stream::connect(imp::name(&endpoint.0)?).await?;
    if imp::peer_is(&stream, &user::current())? {
        Ok(stream)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the server is not this user's",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn endpoint(tag: &str) -> (tempfile::TempDir, Endpoint) {
        let dir = tempfile::Builder::new().prefix(tag).tempdir().unwrap();
        let endpoint = Endpoint::new(dir.path()).unwrap();
        (dir, endpoint)
    }

    #[test]
    fn a_record_names_the_same_endpoint() {
        let (_dir, endpoint) = endpoint("sv-ipc-record");
        assert_eq!(
            Endpoint::from_record(&endpoint.to_record()).unwrap(),
            endpoint
        );
    }

    #[test]
    fn each_data_directory_has_its_own_endpoint() {
        let (_a, one) = endpoint("sv-ipc-a");
        let (_b, two) = endpoint("sv-ipc-b");
        assert_ne!(one.to_record(), two.to_record());
    }

    #[tokio::test]
    async fn this_user_connects_and_talks() {
        let (_dir, endpoint) = endpoint("sv-ipc-talk");
        let listener = listen(&endpoint).await.unwrap();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4];
            stream.read_exact(&mut buffer).await.unwrap();
            stream.write_all(&buffer).await.unwrap();
            stream.flush().await.unwrap();
            let _ = stream.read(&mut buffer).await;
        });
        let mut client = connect(&endpoint).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_second_listener_on_the_same_endpoint_is_refused() {
        let (_dir, endpoint) = endpoint("sv-ipc-twice");
        let _first = listen(&endpoint).await.unwrap();
        let error = listen(&endpoint).await.err().expect("the name is taken");
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn nobody_is_listening() {
        let (_dir, endpoint) = endpoint("sv-ipc-none");
        assert!(connect(&endpoint).await.is_err());
    }
}
