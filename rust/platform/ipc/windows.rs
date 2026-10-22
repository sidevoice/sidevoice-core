//! Windows: an endpoint is a named pipe, which has no file.

use super::{Endpoint, SocketFile};
use std::path::PathBuf;

impl SocketFile for Endpoint {
    fn socket_file(&self) -> Option<PathBuf> {
        None
    }
}
