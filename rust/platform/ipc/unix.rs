//! Unix: a socket file (`0600`), the peer's uid from `SO_PEERCRED`/`getpeereid`, and a stale file replaced.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};

use crate::platform::user::UserId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Endpoint(PathBuf);

impl Endpoint {
    pub(super) fn new(data_dir: &Path) -> io::Result<Self> {
        Self::from_path(&data_dir.join("local.sock"))
    }

    pub(super) fn from_record(record: &str) -> io::Result<Self> {
        Self::from_path(Path::new(record))
    }

    pub(super) fn from_path(path: &Path) -> io::Result<Self> {
        std::path::absolute(path).map(Self)
    }

    pub(super) fn to_record(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    pub(super) fn socket_path(&self) -> Option<&Path> {
        Some(&self.0)
    }
}

pub(super) fn name(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    endpoint.0.clone().to_fs_name::<GenericFilePath>()
}

pub(super) async fn listen(endpoint: &Endpoint) -> io::Result<(Listener, Guard)> {
    remove_stale(&endpoint.0).await?;
    let listener = ListenerOptions::new()
        .name(name(endpoint)?)
        .reclaim_name(false)
        .create_tokio()
        .map_err(|_| io::Error::from(io::ErrorKind::AddrInUse))?;
    let guard = Guard::new(endpoint.0.clone());
    fs::set_permissions(&endpoint.0, fs::Permissions::from_mode(0o600))?;
    Ok((listener, guard))
}

/// Whether the other end of `stream` is `user`: the effective uid of the peer, as the kernel recorded it.
pub(super) fn peer_is(stream: &Stream, user: &UserId) -> io::Result<bool> {
    Ok(same_user(stream.peer_creds()?.euid(), user.raw()))
}

fn same_user(peer: Option<u32>, user: u32) -> bool {
    peer == Some(user)
}

/// A socket file nobody answers on is replaced; anything else at the path, or a live socket, is `AddrInUse`.
async fn remove_stale(path: &Path) -> io::Result<()> {
    let in_use = || io::Error::from(io::ErrorKind::AddrInUse);
    let Ok(found) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !found.file_type().is_socket() {
        return Err(in_use());
    }
    let endpoint = Endpoint(path.to_path_buf());
    match Stream::connect(name(&endpoint)?).await {
        Ok(_) => Err(in_use()),
        Err(error)
            if error.kind() == io::ErrorKind::ConnectionRefused
                || error.kind() == io::ErrorKind::NotFound =>
        {
            fs::remove_file(path)
        }
        Err(_) => Err(in_use()),
    }
}

/// Removes the socket file on drop, unless another process has since replaced it.
pub(super) struct Guard {
    path: PathBuf,
    inode: Option<u64>,
}

impl Guard {
    fn new(path: PathBuf) -> Self {
        let inode = fs::metadata(&path).ok().map(|meta| meta.ino());
        Self { path, inode }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.inode.is_some()
            && fs::metadata(&self.path).ok().map(|meta| meta.ino()) == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_same_uid_is_this_user() {
        assert!(same_user(Some(1000), 1000));
        assert!(!same_user(Some(1001), 1000));
        assert!(!same_user(Some(0), 1000));
        assert!(!same_user(None, 1000));
    }

    #[tokio::test]
    async fn a_socket_nobody_answers_on_is_replaced_and_its_file_goes_with_the_listener() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(dir.path()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&endpoint.0).unwrap());
        assert!(endpoint.0.exists(), "a dead socket file is left behind");
        let (listener, guard) = listen(&endpoint).await.unwrap();
        drop(listener);
        drop(guard);
        assert!(!endpoint.0.exists());
    }

    #[tokio::test]
    async fn what_is_not_a_socket_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::new(dir.path()).unwrap();
        fs::write(&endpoint.0, b"mine").unwrap();
        let error = listen(&endpoint).await.err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(endpoint.0.exists());
    }
}
