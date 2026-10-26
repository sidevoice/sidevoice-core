# Held-root removal

`removal::Removal` extends the existing `fs::HeldDirectory`; callers open it with `Directory::open` and their
required `Access`. The capability retains all ancestors and refuses replacements. It never creates a root. Roots
reached through Windows junctions/redirected profiles are refused: choose an ordinary unredirected directory.
The established Unix system-owned ancestor-link allowance (for example macOS `/var`) is unchanged.

`Directory::open` follows trusted (same-user) links, so a root validated by path and then opened can be swapped
for a link in between. When the directory to remove from must be the one beneath an already held root, descend
with `Directory::open_beneath(relative, access)` instead: each component is opened relative to the previous handle
without resolving any link (Unix `openat` with `O_NOFOLLOW | O_DIRECTORY`; Windows `NtCreateFile` relative to the
parent handle with `FILE_OPEN_REPARSE_POINT`), links and reparse points are refused, and every component must be
on the held root's device and mount (Linux mount ID, macOS mounted-at path, Windows volume): a same-device bind
mount below the root is refused, while the root itself may be a mount point. The result holds the root's ancestry
plus the new components, so `Removal` works on it unchanged. The starting root is still selected by
`Directory::open`; descending only removes the race below it.

On Windows, removal also opens every entry relative to its held parent's handle, never by path: an empty held
directory can become a junction in place (no share mode prevents setting a reparse point), and a path-based open
would follow it. Listing a directory's entries is still by path, so the listing is untrusted and may be stale or
redirected; only the names it yields are used, each opened relative to the held parent. Removal therefore stays
contained beneath the allowlisted subtree whatever the listing says: a listed name that also exists in the held
directory opens and deletes the real child there, never the redirected one; a name that does not exist there is
skipped. An unreliable listing can omit real entries, leaving partial progress (a directory left non-empty, whose
own deletion then fails); retry with the same allowlist.

```rust
use std::ffi::OsStr;
use sidevoice_platform::fs::{Access, Directory, HeldDirectory};
use sidevoice_platform::removal::Removal;
# fn example(root: &std::path::Path) -> std::io::Result<()> {
let directory = HeldDirectory::open(root, Access::Private)?;
let allowed = [OsStr::new("cache"), OsStr::new("staged-release")];
directory.check_contents(&allowed)?; // optional before stopping writers
// Stop writers and hold the application's transaction lock before mutation.
directory.remove_contents(&allowed)?;
# Ok(()) }
```

The list authorizes immediate child entries and their subtrees, in order. Every name is validated before any
mutation; every tree is preflighted, then rechecked during deletion. On Windows each selected opened handle must
report the exact allowlisted long name: 8.3 aliases and case variants cannot authorize an unlisted entry. Unlisted
siblings and the root stay. A missing selected entry is success; a directory appearing concurrently or a failure can leave partial progress. Callers keep
their recovery state until the operation succeeds and can repeat the same list. A live writer may make preflight
fail; the caller owns bounded retry and service shutdown. Preflight itself never claims or writes anything.

The walk is extracted from the connector uninstall mechanism, not its application policy. Unix retains descriptor-
relative no-follow operations, directory identity checks, a depth limit, and device/mount checks. Linux's mount ID
from `/proc/self/fdinfo` detects same-device bind mounts, including files; macOS uses `fstatfs`'s mounted-at identity.
An unavailable mount probe fails closed. Windows retains held ancestors without delete sharing and opens each
child with `OPEN_REPARSE_POINT`. Symlinks, junctions and other reparse entries are unlinked, never traversed. The
handle identifies the object deleted. Ordinary descendant directories must be this user's and not writable by
others. Shared hard-link payloads are not rewritten or truncated.

Windows disposition with `IGNORE_READONLY_ATTRIBUTE` removes a read-only name without clearing attributes on an
external hard link. Unsupported filesystems fall back to ordinary disposition, which can refuse read-only files.
Sharing violations and other native errors are returned unchanged; they are not all classified as “in use”. A
successful disposition can remain delete-pending while external handles are open. No process is killed and nothing
is scheduled for reboot. The caller must not equate success with reclaimed disk space.

## Consumer migration (separate review PRs)

The desktop supplies its own allowlist and owns reset sequencing, recovery and the decision to remove shared
models. The connector continues to preserve Models, `node-stopped.json`, and foreign-file behavior. Neither policy
is hard-coded here. No consumer is migrated by this extraction.

Connector migration must retain its claim-by-rename and same-inode claim verification, recovery executable and
record, claimed release naming, staged retries and service/agent operations. Open the verified claim directory as a
`HeldDirectory` and pass the known claimed entries to `remove_contents`; clean recovery only after all selected
payloads are gone. Where a live preflight races service writes, preserve the connector's bounded retry policy.
Claim publication and final empty-root removal remain caller operations in this PR; the primitive never renames or
deletes the held root, which would invalidate the capability. Review any separate-root/mount layout before adapting
its claim step. Keep lock cleanup separate from bulk payload cleanup: retire other own lock files first and unlink
`install.lock` last while held, with no later lock unlink. Every acquirer must still compare its acquired lock handle
against the current path with `same-file` and retry retired/delete-pending candidates. A held lock is not a lease on
its pathname after unlink.

## Libraries and validation

No new dependency. Maintained `rustix` replaces the connector's raw Unix calls; existing shared fs uses `winapi-util`
and `windows-sys` for identity and ACLs. `cap-std::Dir` was evaluated: its relative operations are useful, but its
recursive remover does not supply Sidevoice's ownership, no-mount-crossing and replaced-ancestor refusal contract.
`std::fs::remove_dir_all` similarly provides symlink-race protection but not those additional checks. Keep the policy
walk over existing bindings rather than add a second directory abstraction. Windows' narrowly scoped unsafe
operations are `SetFileInformationByHandle` and `GetFinalPathNameByHandleW`: std has no checked-handle name or
deletion method and no option to ignore read-only attributes without changing an external hard link.

References: [cap-std directory API](https://docs.rs/cap-std/latest/cap_std/fs/struct.Dir.html),
[std recursive removal](https://doc.rust-lang.org/std/fs/fn.remove_dir_all.html),
[Windows handle disposition](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle),
[Windows normalized handle name](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfinalpathnamebyhandlew).

Tests live in `removal.rs` and run in the existing Linux/macOS/Windows workspace CI matrix. They cover allowlist and
name validation, preflight without mutations, retry, retained recovery/locks, links, external hard links, read-only
attributes and replaced ancestry. Windows-only fixtures exercise junctions, denied delete sharing and alias names
through the same public interface. Windows CI enables 8.3 names on the test temp volume and sets
`SIDEVOICE_REQUIRE_8DOT3=1`, making an unavailable alias fixture a failure. A dedicated `--nocapture` step prints
an evidence line after all alias assertions pass. Locally, with that variable absent, the fixture can skip with an
explicit reason when the volume reports short names disabled. Tests do not claim to simulate every filesystem's delete-pending timing or
require privileged mount creation.
