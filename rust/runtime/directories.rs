//! The data and socket directories, held exclusively for the life of the process.

use std::fs::{self, File};

use super::failure::StartFailure;
use super::Config;
use crate::storage::PrivateDir;

pub(super) struct Directories {
    pub(super) data: PrivateDir,
    _locks: (File, Option<File>),
}

impl Directories {
    /// Open both directories privately and lock each once; a held lock means a Core is running.
    pub(super) fn lock(config: &Config) -> Result<Self, StartFailure> {
        let unsafe_directory = |_| StartFailure::new("directory", "identity.unsafe-directory");
        let data = PrivateDir::open(&config.data_dir).map_err(unsafe_directory)?;
        let socket_parent = config
            .socket
            .parent()
            .ok_or_else(|| StartFailure::new("directory", "identity.unsafe-directory"))?;
        let socket = PrivateDir::open(socket_parent).map_err(unsafe_directory)?;
        let data_lock = lock(&data)?;
        let socket_lock =
            if fs::canonicalize(data.path()).ok() == fs::canonicalize(socket.path()).ok() {
                None
            } else {
                Some(lock(&socket)?)
            };
        Ok(Self {
            data,
            _locks: (data_lock, socket_lock),
        })
    }
}

fn lock(dir: &PrivateDir) -> Result<File, StartFailure> {
    dir.lock()
        .map_err(|_| StartFailure::new("bind", "start.failed"))?
        .ok_or_else(StartFailure::running)
}
