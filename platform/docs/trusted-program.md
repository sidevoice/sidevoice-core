# Trusted Sidevoice programs

`sidevoice_platform::fs::{Files, Fs}` owns `Files::trusted_program(&path) -> io::Result<PathBuf>`.
It checks a recorded Sidevoice program without creating files or repairing access. It is not a validator for
Claude Code, Codex or other agent CLIs: those remain trusted like the user's PATH.

Pass an absolute path without parent (`..`) components. On success, launch the **returned** path with separate
argv elements, immediately after validation. Preserve `NotFound` (missing installation) separately from
`PermissionDenied` (unsafe installation) and `InvalidInput` (invalid command path). Consumers map these IO errors
to their own translated refusals; do not fall back to PATH when a recorded command is unsafe.

| Check | Unix | Windows |
|---|---|---|
| File | Regular, root/current-user owned, not group/other writable, at least one execute bit, no sticky bit | Regular `.exe` (case-insensitive), current-user/SYSTEM/Administrators/TrustedInstaller owned, no other-user write/takeover rights |
| Ancestors | Held descriptors, root/current-user ownership, no group/other write except sticky directories | Every ancestor held without delete sharing, trusted owner/DACL, no reparse points |
| Links | Resolve once, then check the resolved ancestry and no-follow file; never launch the original link | Refuse symbolic links, junctions and other reparse points before canonicalizing |
| Hard links | Allowed for programs; access checks apply to the shared file | Allowed, including system-install hard links; access checks apply to the shared file |

For a redirected Windows install, reinstall into an ordinary, unredirected directory. Do not canonicalize the
input before validation: that would conceal the junction being refused. No elevation or ACL repair is attempted.
Private records retain their existing current-user and single-link checks; accepting a system program does not
relax record access.

## Limits

This boundary excludes malicious code running as this user and administrators. The returned path is **not** an
execution capability: validation releases its handles, and ordinary Unix path launch and Windows CreateProcess
reopen by path. Revalidation narrows the interval but cannot prevent same-user/admin replacement or in-place edits.
Interpreters, shared libraries, loader search paths and program behavior are not validated. Permission checks do not
prove publisher identity, integrity or even that a payload is loadable; the OS loader still decides that. Downloads
continue to need their existing archive integrity checks.

Windows deliberately accepts `.exe`, the format Sidevoice ships. It does not use mutable PATHEXT or accept batch
scripts requiring shell interpretation. Broader `.com`/script compatibility is an operator decision before a future
consumer relies on it. Unix adds an execute-bit check to the desktop's prior ownership/mode validation so an ordinary
non-executable record is refused before launch.

## Implementation and adoption

The implementation extends the held-directory mechanism from core#190. Its internal program-directory policy checks
system-owned leaves with the existing ancestor policy; the public `Directory`/`Access` contract is unchanged. Unix
uses the same rustix no-follow relative child opener as records. Windows reuses handle owner/DACL checks and file
identity; there are no new dependencies or unsafe blocks. `cap-std` was evaluated: its directory capabilities do not
supply Sidevoice's owner/DACL policy, and replacing the already-tested held-directory implementation adds no missing
capability here ([Dir API](https://docs.rs/cap-std/latest/cap_std/fs/struct.Dir.html)).

This block adds the shared owner, without migrating consumers. After a core release containing it, desktop's
local-host must pin that release, import/re-export this operation through its platform module, replace
`trusted::executable`, and remove the Unix implementation. Its record checks migrate separately to `Directory`.
The connector at `208e2c951e0b21d87cd0936fcbf556411884cb9f` has no remaining executable-permission validator to extract;
its Sidevoice launch sites can adopt this operation in their later migration. Do not reintroduce removed agent
checks or duplicate this policy in either consumer.

Tests live beside `Fs` in `fs.rs` and run through the existing CI workspace matrix on Linux, macOS and Windows.
They cover real system executables, user installs, readable parents, unsafe payloads/ancestors, absence, invalid
paths, hard links, replacements and changed links. OS-only cases cover Unix execute/sticky bits and special files,
and Windows aliases, scripts and junctions. Existing held-directory race and record tests exercise the reused
mechanism. These are filesystem validation tests, not proof of descriptor-bound execution or code signing.
