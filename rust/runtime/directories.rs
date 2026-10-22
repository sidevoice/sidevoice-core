//! The data directory, held exclusively for the life of the process.

use std::fs::File;

use super::failure::StartFailure;
use super::Config;
use crate::storage::PrivateDir;

pub(super) struct Directories {
    pub(super) data: PrivateDir,
    _lock: File,
}

impl Directories {
    /// Open the data directory privately and lock it; a held lock means a Core is running. The local channel's
    /// endpoint is derived from it (`Endpoint::new(data_dir, "local")`), so this one lock also makes it one Core's.
    pub(super) fn lock(config: &Config) -> Result<Self, StartFailure> {
        let data = PrivateDir::open(&config.data_dir)
            .map_err(|_| StartFailure::new("directory", "identity.unsafe-directory"))?;
        let lock = lock(&data)?;
        Ok(Self { data, _lock: lock })
    }
}

fn lock(dir: &PrivateDir) -> Result<File, StartFailure> {
    dir.lock()
        .map_err(|_| StartFailure::new("bind", "start.failed"))?
        .ok_or_else(StartFailure::running)
}
