//! Allowlisted tree removal through an existing [`HeldDirectory`]. Application root selection, claim-by-rename,
//! recovery records and lock ordering belong to the caller; this module owns only the no-follow walk and deletion.

#[cfg(unix)]
#[path = "removal/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "removal/windows.rs"]
mod imp;

use crate::fs::{Directory, HeldDirectory};
use std::ffi::OsStr;
use std::io;

/// Removal under a directory opened by [`Directory::open`], with the same held ancestry and access checks.
///
/// Only explicitly allowed immediate children are selected. A selected directory authorizes its entire tree;
/// unlisted siblings and the held root itself stay. Links/reparse points are removed as entries, never traversed.
/// Mount boundaries and unsafe or replaced directories are refused. Hard-linked file contents and permissions
/// are never changed. No application names (Models, recovery records, locks) are built into this capability.
///
/// Callers stop writers and retain their transaction locks. This is not a transaction: an OS error or concurrent
/// change can leave a partially removed tree. Keep recovery state outside the allowlist until success, then retry
/// with the same list. Missing selected children are success. Same-user/admin mutation is outside the trust boundary.
/// Import `Removal` alongside `Directory` and `HeldDirectory`; there is no second root-opening implementation.
pub trait Removal: Directory {
    /// Checks the selected trees without deleting anything. All names must be single ordinary child components
    /// (the same syntax as `Directory::read`); Windows also requires the exact on-disk long name, rejecting 8.3
    /// aliases and case variants. The whole list is validated before deletion. Directories must belong
    /// to this user and not be writable by others. Checks are repeated during removal; preflight is not a lease.
    /// A live writer can cause a refusal; the caller decides whether to retry after stopping it.
    fn check_contents(&self, allowed: &[&OsStr]) -> io::Result<()>;

    /// Preflights the whole allowlist, then removes children in the supplied order, rechecking ancestry and identity.
    /// The root remains held and usable. No directory creation, claim naming, recovery cleanup or root removal occurs.
    ///
    /// Windows sharing violations, access denial and delete-pending failures retain their OS error codes. Success
    /// means the OS accepted deletion, not that every external handle has closed or disk space has been reclaimed.
    /// Read-only files are unlinked without changing attributes (including attributes of external hard links);
    /// filesystems lacking that operation may refuse them. Never retries, kills a process or schedules reboot deletion.
    ///
    /// For uninstall, callers must keep Models and recovery state out of this list, and retire `install.lock` last
    /// while its handle is held. Lock acquisition still requires same-file identity checking after acquisition;
    /// this primitive neither acquires locks nor makes a retired lock inode current again.
    fn remove_contents(&self, allowed: &[&OsStr]) -> io::Result<()>;
}

fn refused(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why)
}

// Implementations use the private state of the existing capability, never expose raw handles to consumers.
impl Removal for HeldDirectory {
    fn check_contents(&self, allowed: &[&OsStr]) -> io::Result<()> {
        imp::contents(&self.0, allowed, false)
    }

    fn remove_contents(&self, allowed: &[&OsStr]) -> io::Result<()> {
        self.check_contents(allowed)?;
        imp::contents(&self.0, allowed, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{Access, Create, Files, Fs, TestFiles};
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};

    fn fixture() -> (tempfile::TempDir, PathBuf, HeldDirectory) {
        let temp = tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        Files::shut_others_out(temp.path());
        let root = temp.path().join("owned");
        Files::create_private_dir(&root).unwrap();
        let held = HeldDirectory::open(&root, Access::Private).unwrap();
        (temp, root, held)
    }

    fn put(path: &Path) {
        Files::create_private(path, b"keep contents intact").unwrap();
    }

    #[test]
    fn removes_only_allowed_trees_and_keeps_root_usable() {
        let (_temp, root, held) = fixture();
        Files::create_private_dir(&root.join("selected")).unwrap();
        put(&root.join("selected/payload"));
        for name in ["Models", "recovery", "install.lock", "unknown"] {
            put(&root.join(name));
        }
        held.check_contents(&[OsStr::new("selected")]).unwrap();
        assert!(root.join("selected/payload").exists());
        held.remove_contents(&[OsStr::new("selected"), OsStr::new("absent")])
            .unwrap();
        held.remove_contents(&[OsStr::new("selected")]).unwrap();
        assert!(!root.join("selected").exists());
        for name in ["Models", "recovery", "install.lock", "unknown"] {
            assert_eq!(fs::read(root.join(name)).unwrap(), b"keep contents intact");
        }
        held.write(OsStr::new("retry.json"), b"still held").unwrap();
    }

    #[test]
    fn removal_through_a_descended_directory_deletes_only_inside_it() {
        let (_temp, root, held) = fixture();
        let models = root.join("Models");
        Files::create_private_dir(&models).unwrap();
        for dir in [&models.join("blobs"), &root.join("blobs")] {
            Files::create_private_dir(dir).unwrap();
            put(&dir.join("payload"));
        }
        put(&models.join("keep"));
        let beneath = held
            .open_beneath(Path::new("Models"), Access::Private)
            .unwrap();
        beneath.remove_contents(&[OsStr::new("blobs")]).unwrap();
        assert!(!models.join("blobs").exists());
        assert!(models.join("keep").exists());
        assert!(root.join("blobs").join("payload").exists());
        // The root stays usable, and so does the descended directory.
        held.check_contents(&[OsStr::new("blobs")]).unwrap();
        beneath.remove_contents(&[OsStr::new("keep")]).unwrap();
        assert!(!models.join("keep").exists());
    }

    #[test]
    fn empty_allowlist_does_nothing_and_all_names_validate_before_deletion() {
        let (_temp, root, held) = fixture();
        put(&root.join("selected"));
        held.remove_contents(&[]).unwrap();
        for invalid in ["", ".", "..", "../outside", "nested/child", "bad\0name"] {
            assert_eq!(
                held.remove_contents(&[OsStr::new("selected"), OsStr::new(invalid)])
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(root.join("selected").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_removes_literal_backslash_names_without_treating_them_as_paths() {
        let (_temp, root, held) = fixture();
        Files::create_private_dir(&root.join("session")).unwrap();
        put(&root.join("session/backup"));
        let tree = root.join(r"session\backup");
        Files::create_private_dir(&tree).unwrap();
        put(&tree.join(r"nested\payload"));
        put(&root.join(r"record\backup"));
        let allowed = [OsStr::new(r"session\backup"), OsStr::new(r"record\backup")];
        held.check_contents(&allowed).unwrap();
        assert!(tree.join(r"nested\payload").exists());
        held.remove_contents(&allowed).unwrap();
        assert!(!tree.exists());
        assert!(!root.join(r"record\backup").exists());
        assert_eq!(
            fs::read(root.join("session/backup")).unwrap(),
            b"keep contents intact"
        );
    }

    #[test]
    fn links_are_unlinked_without_visiting_their_targets() {
        let (temp, root, held) = fixture();
        let outside = temp.path().join("outside");
        Files::create_private_dir(&outside).unwrap();
        put(&outside.join("payload"));
        Files::symlink(&outside, root.join("directory-link"));
        Files::symlink(outside.join("payload"), root.join("file-link"));
        held.remove_contents(&[OsStr::new("directory-link"), OsStr::new("file-link")])
            .unwrap();
        assert!(fs::symlink_metadata(root.join("directory-link")).is_err());
        assert!(fs::symlink_metadata(root.join("file-link")).is_err());
        assert_eq!(
            fs::read(outside.join("payload")).unwrap(),
            b"keep contents intact"
        );
    }

    #[test]
    fn nested_links_and_external_hard_links_keep_their_payloads() {
        let (temp, root, held) = fixture();
        let outside = temp.path().join("outside");
        put(&outside);
        Files::create_private_dir(&root.join("tree")).unwrap();
        Files::symlink(&outside, root.join("tree/link"));
        fs::hard_link(&outside, root.join("tree/hard-link")).unwrap();
        held.remove_contents(&[OsStr::new("tree")]).unwrap();
        assert_eq!(fs::read(outside).unwrap(), b"keep contents intact");
    }

    #[test]
    fn read_only_hard_link_removal_does_not_change_external_attributes() {
        let (temp, root, held) = fixture();
        let outside = temp.path().join("outside");
        put(&outside);
        fs::hard_link(&outside, root.join("selected")).unwrap();
        let original = fs::metadata(&outside).unwrap().permissions();
        let mut permissions = original.clone();
        permissions.set_readonly(true);
        fs::set_permissions(&outside, permissions).unwrap();
        held.remove_contents(&[OsStr::new("selected")]).unwrap();
        assert!(!root.join("selected").exists());
        assert!(fs::metadata(&outside).unwrap().permissions().readonly());
        assert_eq!(fs::read(&outside).unwrap(), b"keep contents intact");
        // The scratch file is outside the primitive's scope; restore its original permissions for TempDir cleanup.
        fs::set_permissions(&outside, original).unwrap();
    }

    #[test]
    fn unsafe_descendants_are_refused_before_any_selected_sibling_is_removed() {
        let (_temp, root, held) = fixture();
        put(&root.join("first"));
        Files::create_private_dir(&root.join("unsafe")).unwrap();
        put(&root.join("unsafe/payload"));
        Files::let_others_change(&root.join("unsafe"));
        assert!(held
            .remove_contents(&[OsStr::new("first"), OsStr::new("unsafe")])
            .is_err());
        assert!(root.join("first").exists());
        assert!(root.join("unsafe/payload").exists());
        Files::shut_others_out(&root.join("unsafe"));
    }

    #[test]
    fn held_root_and_ancestors_cannot_be_redirected_to_replacement_data() {
        for rename_parent in [false, true] {
            let (temp, root, held) = fixture();
            put(&root.join("payload"));
            let source = if rename_parent { temp.path() } else { &root };
            let moved = source.with_extension("moved");
            match fs::rename(source, &moved) {
                Ok(()) => {
                    // Unix allows the rename, but the capability must refuse both the old and replacement trees.
                    Files::create_private_dir(source).unwrap();
                    if rename_parent {
                        Files::create_private_dir(&root).unwrap();
                    }
                    put(&root.join("payload"));
                    assert!(held.remove_contents(&[OsStr::new("payload")]).is_err());
                    assert!(root.join("payload").exists());
                    assert!(moved
                        .join(if rename_parent {
                            "owned/payload"
                        } else {
                            "payload"
                        })
                        .exists());
                    drop(held);
                    fs::remove_dir_all(&moved).unwrap();
                }
                Err(_) => {
                    // Windows denies the rename while the full ancestry is held.
                    assert!(root.join("payload").exists());
                    held.remove_contents(&[OsStr::new("payload")]).unwrap();
                }
            }
        }
    }

    #[test]
    fn rejected_roots_are_not_created_or_followed() {
        let (temp, root, held) = fixture();
        let missing = temp.path().join("missing");
        assert!(HeldDirectory::open(&missing, Access::Private).is_err());
        assert!(!missing.exists());
        // A link is followed only where nobody untrusted could retarget it (the fs rule): not from a loose directory.
        let loose = temp.path().join("loose");
        Files::create_private_dir(&loose).unwrap();
        Files::symlink(&root, loose.join("redirected"));
        Files::let_others_change(&loose);
        assert!(HeldDirectory::open(&loose.join("redirected"), Access::Private).is_err());
        drop(held);
    }

    #[test]
    fn caller_can_retire_a_held_lock_after_payload_and_keep_recovery_until_success() {
        let (_temp, root, held) = fixture();
        put(&root.join("payload"));
        put(&root.join("recovery"));
        let lock = Files::open_private(&root.join("install.lock"), Create::ReadWrite).unwrap();
        lock.lock().unwrap();
        held.remove_contents(&[OsStr::new("payload")]).unwrap();
        assert!(root.join("install.lock").exists());
        assert!(root.join("recovery").exists());
        held.remove_contents(&[OsStr::new("recovery")]).unwrap();
        held.remove_contents(&[OsStr::new("install.lock")]).unwrap();
        // A retained handle can still refer to the retired inode: consumers must compare same-file identity.
        assert!(lock.metadata().is_ok());
        drop(lock);
        assert!(!root.join("install.lock").exists());
    }

    #[test]
    fn depth_limit_is_checked_before_deleting_other_allowed_entries() {
        let (_temp, root, held) = fixture();
        put(&root.join("first"));
        let mut deep = root.clone();
        for _ in 0..66 {
            deep.push("d");
            Files::create_private_dir(&deep).unwrap();
        }
        put(&deep.join("payload"));
        assert!(held
            .remove_contents(&[OsStr::new("first"), OsStr::new("d")])
            .is_err());
        assert!(root.join("first").exists());
        assert!(deep.join("payload").exists());
    }

    #[test]
    fn open_payload_is_removed_or_reports_an_error_that_can_be_retried() {
        let (_temp, root, held) = fixture();
        put(&root.join("payload"));
        let file = File::open(root.join("payload")).unwrap();
        let first = held.remove_contents(&[OsStr::new("payload")]);
        assert!(file.metadata().is_ok());
        drop(file);
        if first.is_err() {
            held.remove_contents(&[OsStr::new("payload")]).unwrap();
        }
        assert!(!root.join("payload").exists());
    }

    // These fixtures use Windows sharing and junction APIs, which cannot be expressed on Unix. Assertions still
    // call the public Removal interface here, and are exercised by the existing native Windows CI job.
    #[cfg(windows)]
    #[test]
    fn windows_in_use_failure_preserves_recovery_and_can_resume_after_partial_deletion() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let (_temp, root, held) = fixture();
        for name in ["first", "busy", "recovery"] {
            put(&root.join(name));
        }
        let busy = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(root.join("busy"))
            .unwrap();
        let error = held
            .remove_contents(&[OsStr::new("first"), OsStr::new("busy")])
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert!(!root.join("first").exists());
        assert!(root.join("busy").exists());
        assert!(root.join("recovery").exists());
        drop(busy);
        held.remove_contents(&[OsStr::new("first"), OsStr::new("busy")])
            .unwrap();
        assert!(root.join("recovery").exists());
    }

    // An empty held root turned into a junction in place: removal opens entries relative to the held handle and
    // rechecks the root, so the junction's target is never reached.
    #[cfg(windows)]
    #[test]
    fn windows_junction_set_on_a_held_root_never_redirects_removal() {
        let (temp, root, held) = fixture();
        let outside = temp.path().join("outside");
        Files::create_private_dir(&outside).unwrap();
        Files::create_private_dir(&outside.join("blobs")).unwrap();
        put(&outside.join("blobs").join("payload"));
        crate::fs::set_junction(&root, &outside);
        assert!(held.remove_contents(&[OsStr::new("blobs")]).is_err());
        assert!(outside.join("blobs").join("payload").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_junction_is_removed_without_touching_its_target() {
        let (temp, root, held) = fixture();
        let outside = temp.path().join("outside");
        Files::create_private_dir(&outside).unwrap();
        put(&outside.join("payload"));
        let junction = root.join("junction");
        assert!(std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .unwrap()
            .status
            .success());
        held.remove_contents(&[OsStr::new("junction")]).unwrap();
        assert!(fs::symlink_metadata(junction).is_err());
        assert!(outside.join("payload").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_delete_pending_error_is_preserved_until_the_last_handle_closes() {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Foundation::GENERIC_READ;
        use windows_sys::Win32::Storage::FileSystem::{
            FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_DISPOSITION_INFO,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let (_temp, root, held) = fixture();
        put(&root.join("pending"));
        let pending = fs::OpenOptions::new()
            .access_mode(GENERIC_READ | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(root.join("pending"))
            .unwrap();
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: a live DELETE handle and a correctly typed/sized buffer; fixture deliberately requests the
        // classic delete-pending behavior rather than POSIX unlink used by modern filesystem removal.
        assert_ne!(
            unsafe {
                SetFileInformationByHandle(
                    pending.as_raw_handle(),
                    FileDispositionInfo,
                    &disposition as *const _ as *const std::ffi::c_void,
                    std::mem::size_of_val(&disposition) as u32,
                )
            },
            0
        );
        let error = held.remove_contents(&[OsStr::new("pending")]).unwrap_err();
        assert!(matches!(error.raw_os_error(), Some(5 | 32 | 303)));
        drop(pending);
        held.remove_contents(&[OsStr::new("pending")]).unwrap();
        assert!(!root.join("pending").exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_short_name_cannot_authorize_an_unlisted_long_name() {
        let (_temp, root, held) = fixture();
        let long = root.join("Models backup");
        Files::create_private_dir(&long).unwrap();
        put(&long.join("payload"));
        put(&root.join("first"));
        let ordinary = long
            .to_str()
            .unwrap()
            .strip_prefix(r"\\?\")
            .unwrap_or(long.to_str().unwrap());
        let output = std::process::Command::new("fsutil")
            .args(["file", "setshortname", ordinary, "MODELS~1"])
            .output()
            .unwrap();
        if !output.status.success() {
            let message = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let lower = message.to_ascii_lowercase();
            if std::env::var_os("SIDEVOICE_REQUIRE_8DOT3").is_none()
                && (lower.contains("disabled") || lower.contains("not enabled"))
            {
                eprintln!("skipping 8.3 alias fixture: short names are disabled on this volume: {message}");
                return;
            }
            panic!("cannot create 8.3 alias fixture: {message}");
        }
        // Prove the fixture really resolves through an 8.3 alias before asserting it is refused.
        assert_eq!(
            fs::read(root.join("MODELS~1/payload")).unwrap(),
            b"keep contents intact"
        );
        assert_eq!(
            held.check_contents(&[OsStr::new("MODELS~1")])
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            held.remove_contents(&[OsStr::new("first"), OsStr::new("MODELS~1")])
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(root.join("first").exists());
        assert_eq!(
            fs::read(long.join("payload")).unwrap(),
            b"keep contents intact"
        );
        // Exact long names work; a literal tilde in an ordinary filename is not itself an alias.
        put(&root.join("literal~1"));
        held.remove_contents(&[
            OsStr::new("first"),
            OsStr::new("Models backup"),
            OsStr::new("literal~1"),
        ])
        .unwrap();
        assert!(!long.exists());
        assert!(!root.join("literal~1").exists());
        eprintln!("8.3 removal regression executed: MODELS~1 resolved to Models backup; alias refused before sibling deletion; exact long name removed");
    }

    #[cfg(windows)]
    #[test]
    fn windows_aliases_are_rejected_before_removal() {
        let (_temp, root, held) = fixture();
        put(&root.join("payload"));
        Files::create_private_dir(&root.join("nested")).unwrap();
        put(&root.join("nested/child"));
        for name in [
            r"nested\child",
            r"\payload",
            r"nested\",
            "payload:stream",
            "payload.",
            "payload ",
            "NUL",
            "COM1",
            "PAYLOAD",
        ] {
            assert!(held
                .remove_contents(&[OsStr::new("payload"), OsStr::new(name)])
                .is_err());
            assert!(root.join("payload").exists());
            assert!(root.join("nested/child").exists());
        }
    }
}
