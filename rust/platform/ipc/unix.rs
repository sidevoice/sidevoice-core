//! Unix: an endpoint is a socket file, and its record is that file's path.

use super::{Endpoint, SocketFile};
use std::path::PathBuf;

impl SocketFile for Endpoint {
    fn socket_file(&self) -> Option<PathBuf> {
        Some(PathBuf::from(self.to_string()))
    }
}
