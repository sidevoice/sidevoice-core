# Shared child execution

`sidevoice_platform::process::{Process, Processes, Child}` extracts the child mechanism from
sidevoice-connector commit `208e2c951e0b21d87cd0936fcbf556411884cb9f`, `connector/src/platform/process.rs`
and the bounded collection in `platform/service.rs`. It uses process-wrap 10 (groups/jobs, creation flags,
kill-on-drop), Tokio (leader/pipe deadlines) and sysinfo (identity observation). Native wrapper types stay private.

`spawn_tree` owns its group/job until the child is dropped, including descendants remaining after leader exit.
`wait`/`try_wait` observe only the leader; apply a Tokio timeout when managing streams yourself. On Unix they
use rustix `waitid(WNOWAIT)` and retain the exited leader until cleanup, reserving its numeric PID/PGID. Cleanup
signals the group once, permanently disarms group signalling, then reaps through Tokio. This prevents a later
`kill_tree` or drop from signalling a reused PGID; callers must not reap the child externally. The tradeoff is
one unreaped leader per retained exited child until `kill_tree` or drop. `output` closes
remaining stdin and concurrently drains both pipes, retains a caller-selected cap per stream, then allows a
caller-selected EOF grace after leader exit. It reports truncation and incomplete EOF separately from status.
Timeout/error/success cleanup kills the tree and waits at most two more seconds to reap the leader. Cancellation
has no reader tasks to leak. Calling `output` takes cleanup ownership even for detached children: dropping its
future, including before the first poll, kills the owned group/job (only the leader for detached Windows children).
Ordinary detached `Child` drop remains non-killing. Streams already taken by a caller remain that caller's responsibility.

`spawn_detached` disables kill-on-drop even if supplied on the command. Unix uses a new process group; Windows
uses detached/new-group/no-window flags. **It does not escape an enclosing Windows Job Object.** Service jobs
that must survive the CLI's cancellation must be started by the service manager, not directly inside its tree.
A Unix descendant that creates its own group/session can also escape tree cleanup. These are lifecycle tools,
not a sandbox. `identity` is an observation, not authorization to signal a stored PID.

The Windows stdio-inheritance repair is the connector's existing three-handle operation; neither stable std nor
process-wrap provides it. Its small unsafe boundary is unchanged, except a failed repair now returns an IO error
before spawn instead of writing to the connector's logger and continuing with potentially held pipes.

## Connector migration after a core release

- Pin the core release and re-export shared `Child`/`Identity`; delegate or inherit `Process`'s shared methods
  from the connector's local trait. Remove local spawn retry, child wrapper, groups/jobs/flags and stdio repair.
- Retain open-file/harness discovery, verified PID signalling, stop/find/age/parent and executable-busy checks
  locally. Shared `identity` does not replace the connector's ownership checks for signalling.
- Replace the service-command reader tasks and `Readers` guard with `Child::output`, supplying its existing
  manager timeout, two-second pipe grace and byte cap. Preserve its output/refusal formatting and map
  `complete == false` to the existing "its output did not end" diagnostic. Agent streaming consumers can keep
  their policy and use the shared child's piped IO and kill/wait operations.
- Remove direct process-wrap/windows dependencies only once all remaining consumers have been checked.
- Keep service integration tests in the connector. In particular, execute actual Task Scheduler jobs and cancel
  the initiating CLI to verify those jobs survive. The core fixture checks job containment and detached pipe
  behavior; it does not claim to test a real service-manager handoff.

The native test fixture replaces the original shell-based descendant cases so the same regressions run on
Linux, macOS and Windows in the existing workspace CI matrix. The Windows-only enclosing-job test documents a
behavior Unix intentionally does not share. No fixture feature or executable is included in release builds.
