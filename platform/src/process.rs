//! Child execution shared by the connector and desktop: groups/jobs, detached children, leader waits and
//! bounded output. Extracted from connector `platform::process` and its service-command runner.
//! Callers own command arguments, environment, stdio and service-manager policy.
//!
//! Detached means no kill on handle drop and no caller-terminal signals. It does **not** escape an enclosing
//! Windows job. Long-lived services must be launched by their service manager, outside the caller's tree.
//! Process identity is an observation, never authorization to signal a recorded PID.

#[cfg(unix)]
#[path = "process/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod imp;

pub use imp::Os as Processes;

use process_wrap::tokio::{ChildWrapper, CommandWrap};
use std::{future::Future, io, process::ExitStatus, time::Duration};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::{
    io::AsyncReadExt,
    process::{ChildStderr, ChildStdin, ChildStdout, Command},
};

/// The child-execution interface. OS implementations keep native flags and `process-wrap` private.
pub trait Process {
    /// Starts a process group (Unix) or windowless Job Object (Windows). Dropping the returned child kills
    /// its tree, even after the leader exits. Descendants that leave the group/job are not contained.
    fn spawn_tree(command: Command) -> impl Future<Output = io::Result<Child>> + Send;

    /// Starts a child in its own process group, without a console on Windows or kill-on-drop on either OS.
    /// The caller owns stdio/environment. This does not escape an enclosing Windows job; use the service
    /// manager for a service that must survive that job. A Windows stdio-inheritance setup failure is returned, naming
    /// the failing call and handle.
    fn spawn_detached(command: Command) -> impl Future<Output = io::Result<Child>> + Send;

    /// Observes a live process's start time and command line, whoever owns it; `None` for absent/zombie or
    /// reserved PIDs. This is a hint, not a race-free process handle or a permission check.
    fn identity(pid: u32) -> Option<Identity> {
        if pid <= 1 {
            return None;
        }
        let pid = Pid::from_u32(pid);
        identity_of(read(ProcessesToUpdate::Some(&[pid])).process(pid)?)
    }
}

/// A process this module started.
///
/// Not `process-wrap`'s own child: its `wait` on a group or a job waits for every process the leader left behind,
/// and a leader's exit is what callers ask about. [`Child::wait`] reports the leader's exit; the tree is stopped
/// with [`Child::kill_tree`]. A tree ([`Processes::spawn_tree`]) is killed whole when its `Child` is dropped: its group
/// (Unix; `KillOnDrop` alone kills only the leader) or its Job Object. A descendant that leaves the group (`setsid`)
/// or the job is not contained.
pub struct Child {
    inner: Box<dyn ChildWrapper>,
    kill_on_drop: bool,
    // Once cleanup starts, group/job signalling is permanently disabled before any leader reaping.
    signal_armed: bool,
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            self.start_kill();
        }
    }
}

impl Child {
    /// The leader PID while it is available.
    pub fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    /// Takes the leader's stdin (when it was piped).
    pub fn stdin(&mut self) -> Option<ChildStdin> {
        self.inner.stdin().take()
    }

    /// Takes the leader's stdout (when it was piped).
    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout().take()
    }

    /// Takes the leader's stderr (when it was piped).
    pub fn stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr().take()
    }

    // The leader alone, beneath the tree's wrapper.
    fn leader(&mut self) -> &mut dyn ChildWrapper {
        self.inner.inner_mut()
    }

    /// The leader's exit, when it has exited. On Unix this observes without reaping while tree signalling
    /// is armed, reserving the leader PID/PGID until `kill_tree` or drop. Do not reap this child externally.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        imp::try_wait(self)
    }

    /// Waits for the leader to exit. Cancel-safe; callers can apply their own Tokio deadline.
    /// This does not wait for descendants or pipe EOF; use `output` for bounded collection.
    /// On Unix the exited leader remains unreaped until cleanup, as in `try_wait`, to prevent PGID reuse.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin());
        imp::wait(self).await
    }

    fn start_kill(&mut self) {
        if self.signal_armed {
            // No await between signalling and disarming: cancellation cannot leave a stale PGID armed.
            let _ = self.inner.start_kill();
            self.signal_armed = false;
        }
    }

    /// Kills the leader and everything it started (its whole group or job, also after the leader has exited) and
    /// reaps the leader, within 2 s. For a detached Windows child there is no owned job: only its leader is killed.
    pub async fn kill_tree(&mut self) {
        self.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.leader().wait()).await;
    }
}

/// Who a process is: what a recorded pid is checked against.
pub struct Identity {
    /// Opaque start time: comparable only with another reading on the same machine.
    pub start: String,
    /// The program and its arguments, joined by spaces.
    pub command: String,
}

// A busy executable (open for writing somewhere: being replaced, or just written while a sibling process forked; std
// names it the same on every OS) runs a moment later: five retries 50 ms apart.
async fn spawn(mut wrap: CommandWrap, tree: bool) -> io::Result<Child> {
    let mut attempt = 0;
    loop {
        match wrap.spawn() {
            Ok(inner) => {
                return Ok(Child {
                    inner,
                    kill_on_drop: tree,
                    signal_armed: true,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy && attempt < 5 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// The processes `pids` name, read now: owner and command line besides what is always read.
fn read(pids: ProcessesToUpdate<'_>) -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(
        pids,
        true,
        ProcessRefreshKind::nothing()
            .with_user(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    system
}

fn identity_of(process: &sysinfo::Process) -> Option<Identity> {
    if process.status() == sysinfo::ProcessStatus::Zombie {
        return None;
    }
    Some(Identity {
        start: process.start_time().to_string(),
        command: process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
    })
}

/// Bounded captured output. Stream truncation and missing EOF are reported separately from leader success.
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Either stream exceeded the per-stream byte limit; excess bytes were drained and discarded.
    pub truncated: bool,
    /// Both streams reached EOF within the post-exit grace.
    pub complete: bool,
}

impl Child {
    /// Consumes a child, closes its remaining stdin, drains both piped streams concurrently, and waits at most
    /// `leader_limit` for the leader, then `pipe_grace` for EOF. Keeps at most `byte_limit` bytes per stream;
    /// absent/taken streams count as empty. Returns `TimedOut` if the leader misses its deadline.
    /// Always kills the remaining tree and allows up to 2 s to reap the leader before returning (also on error).
    /// Calling this takes cleanup ownership even for a detached child: dropping the returned future (including
    /// before its first poll) kills it. Readers are dropped with it; no detached reader tasks remain. On a detached child,
    /// cleanup kills its group on Unix but only its leader on Windows, so use `spawn_tree` for owned commands.
    pub fn output(
        mut self,
        leader_limit: Duration,
        pipe_grace: Duration,
        byte_limit: usize,
    ) -> impl Future<Output = io::Result<Output>> + Send {
        self.kill_on_drop = true;
        async move {
            let result = self.collect(leader_limit, pipe_grace, byte_limit).await;
            self.kill_tree().await;
            result
        }
    }

    async fn collect(
        &mut self,
        leader_limit: Duration,
        pipe_grace: Duration,
        byte_limit: usize,
    ) -> io::Result<Output> {
        drop(self.stdin());
        let stdout = self.stdout();
        let stderr = self.stderr();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut out_truncated = false;
        let mut err_truncated = false;
        let status;
        let complete;
        {
            let pipes = async {
                tokio::try_join!(
                    read_bounded(stdout, &mut out, &mut out_truncated, byte_limit),
                    read_bounded(stderr, &mut err, &mut err_truncated, byte_limit)
                )
            };
            tokio::pin!(pipes);
            let deadline = tokio::time::Instant::now() + leader_limit;
            let collected = tokio::select! {
                result = tokio::time::timeout_at(deadline, self.wait()) => {
                    status = result.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
                    None
                }
                result = &mut pipes => {
                    let result = result?;
                    status = tokio::time::timeout_at(deadline, self.wait()).await
                        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
                    Some(result)
                }
            };
            let collected = match collected {
                Some(result) => Some(result),
                None => tokio::time::timeout(pipe_grace, &mut pipes)
                    .await
                    .ok()
                    .transpose()?,
            };
            complete = collected.is_some();
        }
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
            truncated: out_truncated || err_truncated,
            complete,
        })
    }
}

// Keep draining after the cap so a verbose child cannot block on a full pipe.
async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<R>,
    kept: &mut Vec<u8>,
    truncated: &mut bool,
    limit: usize,
) -> io::Result<()> {
    let Some(mut reader) = reader else {
        return Ok(());
    };
    // On the heap: an array here would sit inside every `output` future (17.5 KB with both streams).
    let mut scratch = vec![0u8; 8192].into_boxed_slice();
    loop {
        let count = reader.read(&mut scratch).await?;
        if count == 0 {
            return Ok(());
        }
        let room = limit.saturating_sub(kept.len()).min(count);
        *truncated |= room < count;
        kept.extend_from_slice(&scratch[..room]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, process::Stdio};
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn command(mode: &str) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--ignored",
            "--exact",
            "--nocapture",
            "process::tests::fixture",
        ])
        .env("SIDEVOICE_PROCESS_TEST", mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        cmd
    }

    // A native child fixture, compiled only into tests, used identically on all three OSes.
    #[test]
    #[ignore = "child fixture"]
    fn fixture() {
        let mode = std::env::var("SIDEVOICE_PROCESS_TEST").unwrap();
        match mode.as_str() {
            "sleeper" => std::thread::sleep(Duration::from_secs(30)),
            "output" => {
                std::io::stdout()
                    .write_all(&vec![b'o'; 256 * 1024])
                    .unwrap();
                std::io::stderr()
                    .write_all(&vec![b'e'; 256 * 1024])
                    .unwrap();
            }
            "stdin" => {
                let input = std::io::read_to_string(std::io::stdin()).unwrap();
                println!("input={input}");
            }
            "descendant" | "orphan" => {
                let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
                cmd.args(["--ignored", "--exact", "process::tests::fixture"])
                    .env("SIDEVOICE_PROCESS_TEST", "sleeper")
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
                // The orphan fixture deliberately exits before its child; the outer test owns tree cleanup.
                #[allow(clippy::zombie_processes)]
                let mut descendant = cmd.spawn().unwrap();
                println!("pid={}", descendant.id());
                std::io::stdout().flush().unwrap();
                if mode == "descendant" {
                    descendant.wait().unwrap();
                }
            }
            "detacher" => {
                tokio::runtime::Runtime::new().unwrap().block_on(async {
                    let mut cmd = command("sleeper");
                    cmd.stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .kill_on_drop(true);
                    let child = Processes::spawn_detached(cmd).await.unwrap();
                    println!("pid={}", child.id().unwrap());
                    drop(child);
                });
            }
            "console" => {
                #[cfg(windows)]
                // SAFETY: observation of this process only; no handle is borrowed or closed.
                assert!(
                    unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() }.is_null()
                );
            }
            "exit" => std::process::exit(7),
            _ => panic!("unknown fixture mode"),
        }
    }

    async fn gone(pid: u32) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while Processes::identity(pid).is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a killed descendant stayed alive");
    }

    // Killed is not enough: the leader must be reaped (no zombie left), which `gone` (identity) cannot tell.
    async fn reaped(pid: u32) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !imp::leader_reaped(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the killed leader was never reaped");
    }

    async fn descendant(child: &mut Child) -> (u32, BufReader<ChildStdout>) {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut reader = BufReader::new(child.stdout().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).await.unwrap() != 0 {
                if let Some(pid) = line.trim().strip_prefix("pid=") {
                    return (pid.parse().unwrap(), reader);
                }
                line.clear();
            }
            panic!("child never reported its descendant");
        })
        .await
        .unwrap()
    }

    // Regression: connector platform/process.rs (including the formerly Unix-only descendant test).
    #[tokio::test]
    async fn a_tree_is_stopped_whole_and_a_dropped_tree_goes_with_its_child() {
        for drop_it in [false, true] {
            let mut child = Processes::spawn_tree(command("descendant")).await.unwrap();
            let pid = child.id().unwrap();
            let (descendant, _output) = descendant(&mut child).await;
            if drop_it {
                drop(child);
            } else {
                child.kill_tree().await;
                assert!(child.try_wait().unwrap().is_some());
            }
            gone(pid).await;
            reaped(pid).await;
            gone(descendant).await;
        }
    }

    #[tokio::test]
    async fn leader_wait_and_try_wait_do_not_wait_for_descendants() {
        let mut child = Processes::spawn_tree(command("orphan")).await.unwrap();
        let (pid, _output) = descendant(&mut child).await;
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        assert!(child.try_wait().unwrap().unwrap().success());
        assert!(Processes::identity(pid).is_some());
        child.kill_tree().await;
        gone(pid).await;
    }

    // Regression: connector platform/service.rs, formerly a Unix shell with an inherited pipe.
    #[tokio::test]
    async fn a_command_whose_descendant_holds_its_pipes_is_not_waited_for() {
        let child = Processes::spawn_tree(command("orphan")).await.unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            child.output(Duration::from_secs(5), Duration::from_millis(200), 4096),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.status.success());
        assert!(!output.complete);
        let pid = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("pid=").map(|pid| pid.parse().unwrap()))
            .unwrap();
        gone(pid).await;
    }

    #[tokio::test]
    async fn output_drains_both_streams_after_the_cap() {
        let child = Processes::spawn_tree(command("output")).await.unwrap();
        let out = child
            .output(Duration::from_secs(10), Duration::from_secs(1), 512)
            .await
            .unwrap();
        assert!(out.status.success());
        assert!(out.complete && out.truncated);
        assert_eq!(out.stdout.len(), 512);
        assert_eq!(out.stderr.len(), 512);
    }

    // Regression: sidevoice-core#198, a 17.5 KB future overflowed a Windows main-thread stack.
    #[tokio::test]
    async fn output_future_keeps_its_read_buffers_off_the_stack() {
        let child = Processes::spawn_tree(command("exit")).await.unwrap();
        let future = child.output(Duration::from_secs(5), Duration::from_secs(1), 4096);
        let size = std::mem::size_of_val(&future);
        assert!(size < 4096, "the output future is {size} bytes");
        drop(future);
    }

    #[tokio::test]
    async fn stdin_can_be_taken_and_output_closes_remaining_stdin() {
        use tokio::io::AsyncWriteExt;
        let mut cmd = command("stdin");
        cmd.stdin(Stdio::piped());
        let mut child = Processes::spawn_tree(cmd).await.unwrap();
        assert!(child.try_wait().unwrap().is_none());
        let mut input = child.stdin().unwrap();
        input.write_all(b"hello").await.unwrap();
        drop(input);
        let out = child
            .output(Duration::from_secs(5), Duration::from_secs(1), 4096)
            .await
            .unwrap();
        assert!(String::from_utf8(out.stdout)
            .unwrap()
            .contains("input=hello"));
        let mut cmd = command("stdin");
        cmd.stdin(Stdio::piped());
        assert!(Processes::spawn_tree(cmd)
            .await
            .unwrap()
            .output(Duration::from_secs(5), Duration::from_secs(1), 4096)
            .await
            .unwrap()
            .status
            .success());
    }

    #[tokio::test]
    async fn timeout_and_cancellation_kill_the_tree() {
        for cancel in [false, true] {
            let mut child = Processes::spawn_tree(command("descendant")).await.unwrap();
            let pid = child.id().unwrap();
            let (descendant, _output) = descendant(&mut child).await;
            if cancel {
                let task = tokio::spawn(child.output(
                    Duration::from_secs(30),
                    Duration::from_secs(1),
                    4096,
                ));
                tokio::task::yield_now().await;
                task.abort();
                assert!(matches!(task.await, Err(error) if error.is_cancelled()));
            } else {
                let error = child
                    .output(Duration::from_millis(100), Duration::from_secs(1), 4096)
                    .await
                    .err()
                    .unwrap();
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            }
            gone(pid).await;
            reaped(pid).await;
            gone(descendant).await;
        }
    }

    #[tokio::test]
    async fn detached_child_outlives_its_handle_and_parent_without_holding_parent_pipes() {
        // The parent itself is detached: a Windows tree intentionally contains all its descendants.
        let parent = Processes::spawn_detached(command("detacher"))
            .await
            .unwrap();
        let output = parent
            .output(Duration::from_secs(5), Duration::from_secs(1), 4096)
            .await
            .unwrap();
        assert!(output.status.success());
        assert!(
            output.complete,
            "detached child inherited the parent's output handles"
        );
        let pid = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix("pid=")
                    .map(|pid| pid.parse::<u32>().unwrap())
            })
            .unwrap();
        let system = read(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]));
        let process = system
            .process(Pid::from_u32(pid))
            .expect("detached child survived");
        assert!(process.kill());
        gone(pid).await;
    }

    #[tokio::test]
    async fn missing_program_is_a_spawn_error_and_nonzero_exit_is_preserved() {
        let error = Processes::spawn_tree(Command::new("sidevoice-no-such-program"))
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let mut child = Processes::spawn_tree(command("exit")).await.unwrap();
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
    }

    // Windows jobs contain a detached descendant; Unix detached children intentionally start another group.
    // This OS-specific contract cannot be asserted identically on Unix.
    #[cfg(windows)]
    #[tokio::test]
    async fn detaching_does_not_escape_an_enclosing_job() {
        let mut tree = Processes::spawn_tree(command("detacher")).await.unwrap();
        let (pid, _output) = descendant(&mut tree).await;
        assert!(tokio::time::timeout(Duration::from_secs(5), tree.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        assert!(Processes::identity(pid).is_some());
        tree.kill_tree().await;
        gone(pid).await;
    }

    #[tokio::test]
    async fn tree_and_detached_launches_have_no_console_window() {
        let mut tree = Processes::spawn_tree(command("console")).await.unwrap();
        assert!(tree.wait().await.unwrap().success());
        let mut detached = Processes::spawn_detached(command("console")).await.unwrap();
        assert!(detached.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn detached_output_cancellation_kills_even_before_first_poll() {
        for unpolled in [true, false] {
            let child = Processes::spawn_detached(command("sleeper")).await.unwrap();
            let pid = child.id().unwrap();
            let output = child.output(Duration::from_secs(30), Duration::from_secs(1), 4096);
            if unpolled {
                drop(output);
            } else {
                assert!(tokio::time::timeout(Duration::from_millis(100), output)
                    .await
                    .is_err());
            }
            gone(pid).await;
            reaped(pid).await;
        }
    }

    async fn cancel_during_pipe_grace(mut child: Child) {
        let leader = child.id().unwrap();
        let (pid, _stdout) = descendant(&mut child).await;
        assert!(child.wait().await.unwrap().success());
        // stderr remains inherited by the descendant, so collection is in its EOF grace after leader exit.
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            child.output(Duration::from_secs(5), Duration::from_secs(30), 4096)
        )
        .await
        .is_err());
        gone(pid).await;
        reaped(leader).await;
    }

    #[tokio::test]
    async fn output_cancellation_during_pipe_grace_cleans_up_descendants() {
        cancel_during_pipe_grace(Processes::spawn_tree(command("orphan")).await.unwrap()).await;
        // Detached Windows children own no Job Object and cannot contain descendants; Unix owns a group.
        #[cfg(unix)]
        cancel_during_pipe_grace(Processes::spawn_detached(command("orphan")).await.unwrap()).await;
    }

    #[tokio::test]
    async fn wait_closes_remaining_stdin() {
        let mut cmd = command("stdin");
        cmd.stdin(Stdio::piped());
        let mut child = Processes::spawn_tree(cmd).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    }

    // Unix-only: WNOWAIT and numeric group identity have no Windows equivalent (jobs are owned handles).
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_reserves_group_identity_until_cleanup_and_never_signals_after_reap() {
        use process_wrap::tokio::{CommandWrapper, KillOnDrop, ProcessGroup};
        use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        #[derive(Debug)]
        struct CountSignals(Arc<AtomicUsize>);
        #[derive(Debug)]
        struct CountedChild(Box<dyn ChildWrapper>, Arc<AtomicUsize>);
        impl CommandWrapper for CountSignals {
            fn wrap_child(
                &mut self,
                inner: Box<dyn ChildWrapper>,
                _: &CommandWrap,
            ) -> io::Result<Box<dyn ChildWrapper>> {
                Ok(Box::new(CountedChild(inner, self.0.clone())))
            }
        }
        impl ChildWrapper for CountedChild {
            // Keep the same leader observation beneath the process-group wrapper.
            fn inner(&self) -> &dyn ChildWrapper {
                self.0.inner()
            }
            fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
                self.0.inner_mut()
            }
            fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
                self.0
            }
            fn start_kill(&mut self) -> io::Result<()> {
                self.1.fetch_add(1, Ordering::SeqCst);
                self.0.start_kill()
            }
        }

        let signals = Arc::new(AtomicUsize::new(0));
        let mut wrap = CommandWrap::from(command("exit"));
        wrap.wrap(ProcessGroup::leader())
            .wrap(KillOnDrop)
            .wrap(CountSignals(signals.clone()));
        let mut child = spawn(wrap, true).await.unwrap();
        let pid = Pid::from_raw(child.id().unwrap() as i32).unwrap();
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
        for _ in 0..2 {
            assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(7));
        }
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        assert_eq!(
            waitid(WaitId::Pid(pid), options)
                .unwrap()
                .unwrap()
                .exit_status(),
            Some(7)
        );
        assert_eq!(signals.load(Ordering::SeqCst), 0);
        child.kill_tree().await;
        assert_eq!(
            waitid(WaitId::Pid(pid), options).unwrap_err(),
            rustix::io::Errno::CHILD
        );
        assert_eq!(signals.load(Ordering::SeqCst), 1);
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
        child.kill_tree().await;
        drop(child);
        assert_eq!(
            signals.load(Ordering::SeqCst),
            1,
            "no group signal after reap"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_observed_signal_status_matches_the_reaped_status() {
        use rustix::process::{kill_process, Pid, Signal};
        use std::os::unix::process::ExitStatusExt;
        let mut child = Processes::spawn_tree(command("sleeper")).await.unwrap();
        kill_process(
            Pid::from_raw(child.id().unwrap() as i32).unwrap(),
            Signal::TERM,
        )
        .unwrap();
        let status = child.wait().await.unwrap();
        assert_eq!(status.signal(), Some(Signal::TERM.as_raw()));
        child.kill_tree().await;
        assert_eq!(child.wait().await.unwrap(), status);
    }

    #[test]
    fn identity_reports_start_and_command_and_rejects_reserved_pids() {
        let identity = Processes::identity(std::process::id()).unwrap();
        assert!(!identity.start.is_empty());
        let exe = std::env::current_exe().unwrap();
        assert!(identity
            .command
            .contains(&*exe.file_stem().unwrap().to_string_lossy()));
        for pid in [0, 1, u32::MAX] {
            assert!(Processes::identity(pid).is_none());
        }
    }
}
