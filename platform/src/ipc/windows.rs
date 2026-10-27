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
use std::time::{Duration, Instant};

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

/// Connects to `endpoint`, the peer unchecked, waiting for a free instance of a busy pipe until `wait` after this
/// call, at most. Opening is always `interprocess`'s Tokio connect without a wait, on the calling task, so the handle
/// goes straight from the open to the runtime's reactor with nothing of ours owning it. Only waiting for a free
/// instance takes a Tokio blocking job (`WaitNamedPipeW`), and that job works to the absolute deadline: when it starts,
/// possibly after waiting in the blocking pool's queue, it waits only for what is left, and not at all once the
/// deadline has passed. So an attempt its caller abandoned costs the pool no time past its deadline. When an instance
/// frees up, the open is tried again (another client may take it first). A pipe still busy at the deadline is
/// `TimedOut`. `interprocess`'s own bounded connect cannot give this: on Windows its local-socket connect ignores the
/// wait mode (`WaitNamedPipe(FOREVER)`), and its Tokio named-pipe connect starts the budget only once its blocking job
/// runs.
pub(super) fn connect(
    endpoint: &Endpoint,
    wait: Duration,
) -> impl Future<Output = io::Result<Stream>> + Send + 'static {
    use interprocess::os::windows::named_pipe::local_socket::tokio::Stream as PipeSocket;
    use interprocess::os::windows::named_pipe::{pipe_mode, tokio::DuplexPipeStream};
    use interprocess::ConnectWaitMode;
    // `WaitNamedPipeW`'s longest finite wait, about 49 days: an `Instant` that far ahead is always representable.
    const LONGEST: Duration = Duration::from_millis(0xFFFF_FFFE);
    let deadline = Instant::now() + wait.min(LONGEST);
    let path = endpoint.0.clone();
    async move {
        loop {
            match DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path_with_wait_mode(
                path.as_str(),
                ConnectWaitMode::Timeout(Duration::ZERO),
            )
            .await
            {
                Ok(pipe) => return Ok(Stream::from(PipeSocket::from(pipe))),
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {}
                Err(error) => return Err(error),
            }
            if deadline <= Instant::now() {
                return Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            let path = path.clone();
            tokio::task::spawn_blocking(move || wait_for_instance(&path, deadline))
                .await
                .map_err(io::Error::other)??;
        }
    }
}

/// Waits, on this (blocking) thread, until an instance of the pipe `path` is free or `deadline` has passed (then
/// `TimedOut`, at once if it already has). A pipe that is gone is its own error.
fn wait_for_instance(path: &str, deadline: Instant) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SEM_TIMEOUT;
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::from(io::ErrorKind::TimedOut));
    }
    let path = widestring::U16CString::from_str(path).map_err(io::Error::other)?;
    // Never 0 (the pipe's own default wait) nor 0xFFFFFFFF (forever): 1 ms to the longest finite wait.
    let millis = u32::try_from(left.as_millis())
        .unwrap_or(u32::MAX)
        .clamp(1, 0xFFFF_FFFE);
    // SAFETY: `path` is a NUL-terminated wide string that outlives the call; no handle is involved.
    if unsafe { WaitNamedPipeW(path.as_ptr(), millis) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_SEM_TIMEOUT as i32) {
        return Err(io::Error::from(io::ErrorKind::TimedOut));
    }
    Err(error)
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

    /// A runtime with a single blocking thread: a busy-pipe wait that outlives its deadline holds every later blocking
    /// job back, which a sentinel job then measures.
    fn one_blocking_thread() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
    }

    /// The caller gives up on each attempt long before its budget (its own timeout drops the future). The first
    /// attempt's wait ends at its deadline; the others, queued behind it, reach the thread at or after theirs (they
    /// were 20 ms apart) and wait for what is left at most. Five full waits one after another would take five budgets.
    #[test]
    fn abandoned_attempts_on_a_busy_pipe_give_their_blocking_thread_back() {
        let runtime = one_blocking_thread();
        runtime.block_on(async {
            let (_dir, endpoint, _listener, _holder) = busy_pipe("abandoned").await;
            let wait = Duration::from_millis(500);
            let started = Instant::now();
            for _ in 0..5 {
                let attempt = Channels::connect_within(&endpoint, wait);
                assert!(tokio::time::timeout(Duration::from_millis(20), attempt)
                    .await
                    .is_err());
            }
            tokio::time::timeout(Duration::from_secs(10), tokio::task::spawn_blocking(|| ()))
                .await
                .expect("an abandoned attempt still holds the blocking thread")
                .unwrap();
            let elapsed = started.elapsed();
            assert!(
                elapsed < 2 * wait,
                "the blocking thread was back after {elapsed:?}"
            );
            // And the pipe, still busy, is still refused within the budget.
            let error = Channels::connect_within(&endpoint, wait).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
    }

    /// One deadline from the call, however long the attempt's blocking job waits in the queue: the only blocking
    /// thread is held behind a gate while an attempt on a busy pipe is queued and its budget runs out. Once the gate
    /// opens, the attempt's job finds its deadline gone and does not start a wait of its own, so a sentinel job runs
    /// at once, not a full budget later.
    #[test]
    fn an_attempt_queued_past_its_deadline_does_not_wait_again() {
        let runtime = one_blocking_thread();
        runtime.block_on(async {
            let (_dir, endpoint, _listener, _holder) = busy_pipe("queued").await;
            let (open, gate) = std::sync::mpsc::channel::<()>();
            let held = tokio::task::spawn_blocking(move || gate.recv().unwrap());
            let wait = Duration::from_secs(2);
            let started = Instant::now();
            let error = Channels::connect_within(&endpoint, wait).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
            assert!(
                started.elapsed() < wait + Duration::from_secs(1),
                "{:?}",
                started.elapsed()
            );
            // Its job is still queued behind the gate, its deadline gone.
            open.send(()).unwrap();
            held.await.unwrap();
            let released = Instant::now();
            tokio::time::timeout(Duration::from_secs(10), tokio::task::spawn_blocking(|| ()))
                .await
                .expect("the queued attempt still holds the blocking thread")
                .unwrap();
            let elapsed = released.elapsed();
            assert!(
                elapsed < wait / 2,
                "the queued attempt waited {elapsed:?} after its deadline"
            );
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
    }

    /// A pipe that frees up within the budget is connected, and carries bytes both ways: the server accepts the
    /// holder's connection only after a while (which makes the next instance), then the waiting client's.
    #[tokio::test]
    async fn a_busy_pipe_freed_within_the_budget_connects_and_carries_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_dir, endpoint, listener, holder) = busy_pipe("freed").await;
        let delay = Duration::from_millis(500);
        let server = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _held = listener.accept().await.unwrap();
            let mut stream = listener.accept().await.unwrap();
            let mut line = [0u8; 5];
            stream.read_exact(&mut line).await.unwrap();
            stream.write_all(&line).await.unwrap();
        });
        let started = Instant::now();
        let mut client = Channels::connect_within(&endpoint, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(started.elapsed() >= delay / 2, "{:?}", started.elapsed());
        client.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");
        server.await.unwrap();
        drop(holder);
    }
}
