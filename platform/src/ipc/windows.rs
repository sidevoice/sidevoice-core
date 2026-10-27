//! Windows: a named pipe, `\\.\pipe\sidevoice-<hash of (user name, data dir)>-<name>`. The pipe is created as the
//! first instance (a name already taken is an error) and refusing remote clients, both `interprocess`'s defaults,
//! with a protected descriptor that lets in only its owner (`OW`: the user whose process made it), SYSTEM and
//! Administrators. The peer check: the process at the other end of the pipe belongs to this user.

use super::{Listener, Stream};
use crate::user::{User, UserId};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{GenericFilePath, ListenerOptions, Name, ToFsName};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use sha2::{Digest, Sha256};
use std::fmt;
use std::future::Future;
use std::io;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

const PREFIX: &str = r"\\.\pipe\";

/// Full access for the pipe's owner, SYSTEM and Administrators; nobody else, nothing inherited. Also the descriptor of
/// a process's stop event (`crate::stop`).
pub(crate) const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)";

/// An endpoint: its full pipe name.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint(String);

impl super::Address for Endpoint {
    fn new(dir: &Path, name: &str) -> Endpoint {
        // `dir` itself: a pipe has no path to keep short, so Windows has no runtime directory.
        let dir = super::endpoint_dir(<Os as super::Channel>::runtime_dir().as_deref(), dir);
        let mut hash = Sha256::new();
        hash.update(UserId::name().unwrap_or_default().to_lowercase().as_bytes());
        hash.update([0]);
        hash.update(dir.to_string_lossy().to_lowercase().as_bytes());
        Endpoint(format!(
            "{PREFIX}sidevoice-{}-{name}",
            hex::encode(&hash.finalize()[..8])
        ))
    }
}

// A record is a full local pipe name, `\\.\pipe\...`.
impl FromStr for Endpoint {
    type Err = io::Error;

    fn from_str(record: &str) -> io::Result<Endpoint> {
        if record.len() <= PREFIX.len() || !record.starts_with(PREFIX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a local pipe name: {record:?}"),
            ));
        }
        Ok(Endpoint(record.to_owned()))
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Endpoint {
    /// The endpoint as `interprocess` names it.
    pub(super) fn name(&self) -> io::Result<Name<'static>> {
        self.0.clone().to_fs_name::<GenericFilePath>()
    }
}

/// Windows side of [`super::Channel`].
pub struct Os;

impl super::Channel for Os {
    fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
        let descriptor = widestring::U16CString::from_str(OWNER_ONLY).map_err(io::Error::other)?;
        ListenerOptions::new()
            .name(endpoint.name()?)
            .security_descriptor(SecurityDescriptor::deserialize(&descriptor)?)
            .create_tokio()
            .map(Listener)
    }
}

/// Connects to `endpoint`, the peer unchecked, waiting at most `wait` for a free instance of a busy pipe.
/// `interprocess`'s local-socket connect cannot be bounded here: on Windows it ignores `ConnectOptions::wait_mode` and
/// waits in `WaitNamedPipe(FOREVER)` on a blocking thread. Its named-pipe connect takes a wait mode: with a timeout,
/// each `WaitNamedPipe` on that thread gets what is left of `wait`, so the thread is back once `wait` has passed, and a
/// pipe still busy then is `TimedOut`.
pub(super) fn connect(
    endpoint: &Endpoint,
    wait: Duration,
) -> impl Future<Output = io::Result<Stream>> + Send + 'static {
    use interprocess::os::windows::named_pipe::local_socket::tokio::Stream as PipeSocket;
    use interprocess::os::windows::named_pipe::{pipe_mode, tokio::DuplexPipeStream};
    use interprocess::ConnectWaitMode;
    let path = endpoint.0.clone();
    async move {
        let pipe = DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path_with_wait_mode(
            path,
            ConnectWaitMode::Timeout(wait),
        )
        .await?;
        Ok(Stream::from(PipeSocket::from(pipe)))
    }
}

/// Whether the process at the other end of `stream` is this user's: the pipe's client or server pid, as
/// `interprocess` reports it, is a live process of this user's.
pub(super) fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    let pid = stream
        .peer_creds()?
        .pid()
        .ok_or_else(|| io::Error::other("the peer process is unknown"))?;
    Ok(is_this_users_process(pid))
}

/// Whether `pid` is, now, a live process (not a zombie) of the user this one runs as (`sysinfo`).
fn is_this_users_process(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};
    let owner = |pid: Pid| {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_user(UpdateKind::Always),
        );
        let process = system.process(pid)?;
        (process.status() != ProcessStatus::Zombie)
            .then(|| process.user_id().cloned())
            .flatten()
    };
    // 0 and 1 are never a peer (the idle process, and init in the Unix sense).
    if pid <= 1 {
        return false;
    }
    let Ok(this) = sysinfo::get_current_pid() else {
        return false;
    };
    matches!((owner(Pid::from_u32(pid)), owner(this)), (Some(peer), Some(me)) if peer == me)
}

// Windows only: the pid lookup is the Windows peer check's own helper (Unix asks the kernel for the peer's uid), and
// the generic tests only reach it with this process as the peer; here, the pids that must never pass. And a busy pipe,
// which only Windows waits for: a connect gives it up within its budget, its blocking thread too.
#[cfg(test)]
mod tests {
    use super::super::{Address, Channel as _, Channels};
    use super::*;
    use std::time::Instant;

    #[test]
    fn this_process_is_this_users_and_nothing_else_is() {
        assert!(is_this_users_process(std::process::id()));
        assert!(!is_this_users_process(0));
        assert!(!is_this_users_process(1));
        assert!(!is_this_users_process(u32::MAX));
    }

    /// The pipe's one instance taken: its listener has not accepted (it makes the next instance only on accept), and a
    /// client holds it. Any further client finds the pipe busy, as behind a stalled server or a squatter.
    async fn busy_pipe(name: &str) -> (tempfile::TempDir, Endpoint, Listener, Stream) {
        let dir = super::super::tests::scratch();
        let endpoint = <Endpoint as Address>::new(dir.path(), name);
        let listener = Channels::listen(&endpoint).unwrap();
        let holder = Channels::connect(&endpoint).await.unwrap();
        (dir, endpoint, listener, holder)
    }

    #[tokio::test]
    async fn a_busy_pipe_is_timed_out_within_the_budget_not_waited_for() {
        let (_dir, endpoint, _listener, _holder) = busy_pipe("busy").await;
        let wait = Duration::from_millis(300);
        let started = Instant::now();
        let error = Channels::connect_within(&endpoint, wait).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // No wait at all: refused at once, without a blocking thread.
        let error = Channels::connect_within(&endpoint, Duration::ZERO)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    }

    /// The caller gives up on each attempt long before its budget (its own timeout drops the future), and the
    /// attempt's blocking wait still ends within that budget: with a single blocking thread, a wait left parked would
    /// hold every later blocking task in the queue for good.
    #[test]
    fn abandoned_attempts_on_a_busy_pipe_give_their_blocking_thread_back() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_dir, endpoint, _listener, _holder) = busy_pipe("abandoned").await;
            let wait = Duration::from_millis(200);
            for _ in 0..5 {
                let attempt = Channels::connect_within(&endpoint, wait);
                assert!(tokio::time::timeout(Duration::from_millis(20), attempt)
                    .await
                    .is_err());
            }
            // The five waits run one after another on the one thread, each over within `wait`.
            tokio::time::timeout(Duration::from_secs(10), tokio::task::spawn_blocking(|| ()))
                .await
                .expect("an abandoned attempt still holds the blocking thread")
                .unwrap();
            // And the pipe, still busy, is still refused within the budget.
            let error = Channels::connect_within(&endpoint, wait).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
    }
}
