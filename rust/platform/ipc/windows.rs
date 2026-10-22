//! Windows: a named pipe, owner-only, first instance (a squatter makes `listen` fail), no remote clients (the
//! `interprocess` defaults), and the peer's user read from the token of the pipe client's process.

use std::io;
use std::path::Path;

use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use sha2::{Digest, Sha256};
use widestring::U16CString;

use crate::platform::user::{self, UserId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Endpoint(String);

impl Endpoint {
    pub(super) fn new(data_dir: &Path) -> io::Result<Self> {
        let sid = user::current().sid_string()?;
        let mut hash = Sha256::new();
        hash.update(sid.as_bytes());
        hash.update([0]);
        hash.update(data_dir.as_os_str().as_encoded_bytes());
        let short: String = hash.finalize()[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self(format!(r"\\.\pipe\sidevoice-{short}-core")))
    }

    pub(super) fn from_record(record: &str) -> io::Result<Self> {
        Ok(Self(record.to_owned()))
    }

    pub(super) fn to_record(&self) -> String {
        self.0.clone()
    }

    pub(super) fn socket_path(&self) -> Option<&Path> {
        None
    }
}

pub(super) fn name(endpoint: &Endpoint) -> io::Result<Name<'static>> {
    endpoint.0.clone().to_fs_name::<GenericFilePath>()
}

/// Nothing to clean up: a pipe name dies with its last handle.
pub(super) struct Guard;

pub(super) async fn listen(endpoint: &Endpoint) -> io::Result<(Listener, Guard)> {
    // This user, the system and the administrators; nobody else can open the pipe.
    let me = user::current().sid_string()?;
    let sddl = U16CString::from_str(format!("D:P(A;;GA;;;{me})(A;;GA;;;SY)(A;;GA;;;BA)"))
        .map_err(io::Error::other)?;
    let listener = ListenerOptions::new()
        .name(name(endpoint)?)
        .security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
        .create_tokio()?;
    Ok((listener, Guard))
}

/// Whether the other end of `stream` is `user`: the user of the process at the other end of the pipe.
pub(super) fn peer_is(stream: &Stream, user: &UserId) -> io::Result<bool> {
    let pid = stream
        .peer_creds()?
        .pid()
        .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
    Ok(UserId::of_process(pid)? == *user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pipe_is_named_from_the_user_and_the_directory() {
        let endpoint = Endpoint::new(Path::new(r"C:\data")).unwrap();
        assert!(
            endpoint.0.starts_with(r"\\.\pipe\sidevoice-"),
            "{}",
            endpoint.0
        );
        assert!(endpoint.0.ends_with("-core"));
        assert_ne!(endpoint, Endpoint::new(Path::new(r"C:\other")).unwrap());
    }

    #[tokio::test]
    async fn the_first_instance_is_the_only_one() {
        let endpoint =
            Endpoint::new(Path::new(&format!("C:\\sv-test-{}", std::process::id()))).unwrap();
        let _first = listen(&endpoint).await.unwrap();
        assert!(listen(&endpoint).await.is_err());
    }
}
