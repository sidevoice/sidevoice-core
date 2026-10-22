//! The local channel: the shared crate's ([`sidevoice_platform::ipc`], re-exported whole), plus what only the core
//! needs of an endpoint, [`SocketFile`].

#[cfg(unix)]
#[path = "ipc/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "ipc/windows.rs"]
mod imp;

pub use sidevoice_platform::ipc::*;

use std::path::PathBuf;

/// What only the core asks of an [`Endpoint`], implemented per OS (`ipc/unix.rs`, `ipc/windows.rs`).
pub trait SocketFile {
    /// The socket file the endpoint is, where it is one (Unix: its record is the socket's path); `None` where a
    /// channel has no file (a Windows pipe). The ready file keeps it as its deprecated `socket` field, and the core
    /// holds its directory besides its data directory.
    fn socket_file(&self) -> Option<PathBuf>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_file_is_the_endpoint_in_its_directory_when_there_is_one() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(dir.path(), "local");
        if let Some(socket) = endpoint.socket_file() {
            assert_eq!(socket.parent(), Some(dir.path()));
            assert_eq!(socket.to_str(), Some(endpoint.to_string().as_str()));
        }
    }
}
