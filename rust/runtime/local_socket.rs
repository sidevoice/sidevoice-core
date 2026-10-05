//! The private local Unix socket and its removal on exit.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use super::failure::StartFailure;

/// Bind `path` with owner-only permissions, replacing a stale socket nobody answers on.
pub(super) async fn bind(path: &Path) -> Result<(UnixListener, SocketCleanup), StartFailure> {
    remove_stale(path).await?;
    let listener =
        UnixListener::bind(path).map_err(|_| StartFailure::new("bind", "bind.port-in-use"))?;
    let cleanup = SocketCleanup::new(path.to_path_buf());
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| StartFailure::new("bind", "start.failed"))?;
    Ok((listener, cleanup))
}

async fn remove_stale(path: &Path) -> Result<(), StartFailure> {
    let Ok(found) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !found.file_type().is_socket() {
        return Err(StartFailure::new("bind", "bind.port-in-use"));
    }
    match UnixStream::connect(path).await {
        Ok(_) => Err(StartFailure::new("bind", "bind.port-in-use")),
        Err(error)
            if error.kind() == io::ErrorKind::ConnectionRefused
                || error.kind() == io::ErrorKind::NotFound =>
        {
            fs::remove_file(path).map_err(|_| StartFailure::new("bind", "start.failed"))
        }
        Err(_) => Err(StartFailure::new("bind", "bind.port-in-use")),
    }
}

/// Removes the socket on drop, unless another process has since replaced it.
pub(super) struct SocketCleanup {
    path: PathBuf,
    inode: Option<u64>,
}

impl SocketCleanup {
    fn new(path: PathBuf) -> Self {
        let inode = fs::metadata(&path).ok().map(|meta| meta.ino());
        Self { path, inode }
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if self.inode.is_some()
            && fs::metadata(&self.path).ok().map(|meta| meta.ino()) == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
