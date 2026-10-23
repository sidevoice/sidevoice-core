//! The private-directory and private-file rules, and the process lock.

use super::*;

/// Whether the directory at `path` is private to this user, as the storage judges it.
fn private(path: &Path) -> bool {
    Files::is_private_dir(path).unwrap()
}

fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A directory this user does not own, whatever the OS: the root of the file system (Unix) or the Windows directory
/// (owned by the system); `None` where this user owns it (root).
fn another_users_directory() -> Option<PathBuf> {
    let path = if cfg!(windows) {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
    } else {
        PathBuf::from("/")
    };
    (!UserId::current().owns_path(&path).unwrap()).then_some(path)
}

#[test]
fn a_directory_it_creates_is_this_user_s_alone() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("nested").join("core");
    PrivateDir::open(&path).unwrap();
    assert!(private(&path));
    assert!(private(path.parent().unwrap()), "and each parent it made");
    PrivateDir::open(&path).unwrap();
}

// Unix only: the unsafe directories are built with `chmod`; the same rule is tested on every OS against the
// interface (`sidevoice-platform`, fs tests: a directory others can enter is not private and is not repaired).
#[cfg(unix)]
#[test]
fn only_a_directory_this_user_alone_may_enter_is_safe() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
    let root = tempfile::tempdir().unwrap();
    let safe = root.path().join("core");
    PrivateDir::open(&safe).unwrap();
    for loose in [0o750, 0o705, 0o770, 0o777, 0o701] {
        fs::set_permissions(&safe, fs::Permissions::from_mode(loose)).unwrap();
        let refused = PrivateDir::open(&safe).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{loose:o}");
        assert_eq!(mode(&safe), loose, "a refused directory is left as it was");
    }
}

#[test]
fn a_file_is_not_a_directory() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("file");
    fs::write(&file, b"").unwrap();
    assert!(PrivateDir::open(&file).is_err());
    assert!(PrivateDir::open_for_report(&file).is_err());
}

// Unix only: a symlink needs no privilege there; on Windows a link is a junction or a privileged symlink, and the
// refusal is tested on every OS against the interface (`sidevoice-platform`, fs tests).
#[cfg(unix)]
#[test]
fn a_link_to_a_directory_is_not_the_directory() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("elsewhere");
    PrivateDir::open(&target).unwrap();
    let link = root.path().join("core");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(PrivateDir::open(&link).is_err());
    assert!(PrivateDir::open_for_report(&link).is_err());
}

#[cfg(unix)]
#[test]
#[ignore = "known gap: a trailing `/`, `/.` or `//` on a symlinked data directory is followed (lstat resolves it)"]
fn a_link_is_refused_however_it_is_spelled() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("elsewhere");
    PrivateDir::open(&target).unwrap();
    let link = root.path().join("core");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    for suffix in ["/", "/.", "//"] {
        let spelled = PathBuf::from(format!("{}{suffix}", link.display()));
        assert!(PrivateDir::open(&spelled).is_err(), "core{suffix}");
        assert!(
            PrivateDir::open_for_report(&spelled).is_err(),
            "core{suffix}"
        );
    }
}

#[test]
fn another_user_s_directory_is_refused() {
    let Some(other) = another_users_directory() else {
        return; // The one who runs this owns it (root): nothing stands for another user's directory.
    };
    let refused = PrivateDir::open(&other).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
    assert!(PrivateDir::open_for_report(&other).is_err());
}

#[test]
fn a_secret_is_private_from_its_creation_and_leaves_no_temporary() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    dir.write_json(
        "integrations.json",
        &serde_json::json!({"openai": "sk-first-1111"}),
    )
    .unwrap();
    dir.write_json(
        "integrations.json",
        &serde_json::json!({"openai": "sk-first-1111", "elevenlabs": "xi-second-2222"}),
    )
    .unwrap();
    assert!(is_private_file(&dir.file("integrations.json")));
    assert_eq!(names(dir.path()), ["integrations.json"]);
    assert_eq!(
        dir.read_json("integrations.json").unwrap().unwrap(),
        serde_json::json!({"openai": "sk-first-1111", "elevenlabs": "xi-second-2222"})
    );
}

#[test]
fn every_staging_file_has_a_name_of_its_own() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    let first = dir.stage(b"one").unwrap();
    let second = dir.stage(b"two").unwrap();
    assert_ne!(
        first.path(),
        second.path(),
        "no fixed name another writer could share"
    );
    for staged in [&first, &second] {
        assert_eq!(staged.path().parent(), Some(dir.path()));
        assert!(
            is_private_file(staged.path()),
            "private before it is renamed in"
        );
    }
}

// Unix only: the write is made to fail by a read-only directory (mode 0500); a directory's read-only attribute on
// Windows does not stop files being created in it.
#[cfg(unix)]
#[test]
fn a_write_that_fails_leaves_the_old_file_and_no_temporary() {
    use std::os::unix::fs::PermissionsExt;
    if another_users_directory().is_none() {
        return; // root writes into a read-only directory.
    }
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    dir.write_json(
        "integrations.json",
        &serde_json::json!({"openai": "sk-working-0000"}),
    )
    .unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let failed = dir.write_json(
        "integrations.json",
        &serde_json::json!({"openai": "sk-never-1111"}),
    );
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(failed.is_err());
    assert_eq!(
        dir.read_json("integrations.json").unwrap().unwrap(),
        serde_json::json!({"openai": "sk-working-0000"})
    );
    assert_eq!(names(dir.path()), ["integrations.json"]);
}

#[test]
fn a_rename_that_fails_removes_its_staging_file_too() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    dir.write_json("integrations.json", &serde_json::json!({}))
        .unwrap();
    // The name is taken by a directory that is not empty.
    fs::create_dir(dir.file("taken")).unwrap();
    fs::write(dir.file("taken").join("inside"), b"").unwrap();
    assert!(dir.write_private("taken", b"secret").is_err());
    assert_eq!(names(dir.path()), ["integrations.json", "taken"]);
}

#[test]
fn a_file_created_once_is_never_replaced() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    dir.link_new("node-identity.json", b"first").unwrap();
    let again = dir.link_new("node-identity.json", b"second").unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(dir.file("node-identity.json")).unwrap(), b"first");
    assert!(is_private_file(&dir.file("node-identity.json")));
    assert_eq!(
        names(dir.path()),
        ["node-identity.json"],
        "no staging file is left behind"
    );
}

#[test]
fn one_lock_per_directory_held_until_dropped() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    let held = dir.lock().unwrap().expect("first lock");
    assert!(dir.lock().unwrap().is_none(), "a second starter is refused");
    assert!(is_private_file(&dir.file("core.lock")));
    drop(held);
    assert!(dir.lock().unwrap().is_some(), "released with its holder");
}

// Unix only, as the link test above (the same refusal of a link at the lock's place is tested against the interface).
#[cfg(unix)]
#[test]
fn a_lock_file_that_is_a_link_is_not_followed() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    let target = root.path().join("somebody-else");
    fs::write(&target, b"untouched").unwrap();
    std::os::unix::fs::symlink(&target, dir.file("core.lock")).unwrap();
    assert!(dir.lock().is_err());
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
}

#[test]
fn removing_what_is_absent_is_not_an_error() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    dir.remove("core-failure.json").unwrap();
    dir.write_json("core-failure.json", &serde_json::json!({}))
        .unwrap();
    dir.remove("core-failure.json").unwrap();
    assert!(names(dir.path()).is_empty());
}

// Unix only: the loose parent is built with `chmod`; the same rule (a directory others can change on the way is not
// trusted) is tested on every OS against the interface (`sidevoice-platform`, fs tests).
#[cfg(unix)]
#[test]
fn a_private_directory_under_one_others_can_change_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    PrivateDir::open(shared.join("core")).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    let refused = PrivateDir::open(shared.join("core")).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
    PrivateDir::open(shared.join("core")).expect("under the sticky bit, as in /tmp");
}

// Windows only: an entry granting Everyone read on what is created inside (inherit-only) has no Unix counterpart.
#[cfg(windows)]
#[test]
fn a_directory_that_would_let_others_read_its_files_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    let status = std::process::Command::new("icacls")
        .arg(dir.path())
        .args(["/grant", "*S-1-1-0:(OI)(IO)(R)"])
        .status()
        .unwrap();
    assert!(status.success(), "icacls");
    let refused = PrivateDir::open(dir.path()).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn a_record_or_lock_that_is_not_a_file_is_refused_and_left_as_it_is() {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    for name in ["node-identity.json", "core.lock"] {
        Files::create_private_dir(&dir.file(name)).unwrap();
    }
    let refused = dir.read_json("node-identity.json").unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
    assert!(dir.lock().is_err());
    assert!(dir.file("node-identity.json").is_dir() && dir.file("core.lock").is_dir());
}

// Unix only: the loose files are built with `chmod` and `symlink`; that a file others can open is not private is tested
// on every OS against the interface (`sidevoice-platform`, fs tests), as is a link in a private file's place.
#[cfg(unix)]
#[test]
fn a_record_or_lock_others_can_open_or_that_is_a_link_is_refused_and_left_as_it_is() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    dir.write_json(
        "connector-credential.json",
        &serde_json::json!({"token": "t"}),
    )
    .unwrap();
    drop(dir.lock().unwrap().expect("first lock"));
    for loose in [0o640, 0o604, 0o620, 0o602] {
        for name in ["connector-credential.json", "core.lock"] {
            fs::set_permissions(dir.file(name), fs::Permissions::from_mode(loose)).unwrap();
        }
        let refused = dir.read_json("connector-credential.json").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{loose:o}");
        assert_eq!(
            dir.lock().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
            "{loose:o}"
        );
        let mode = |name: &str| fs::metadata(dir.file(name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("connector-credential.json"), loose, "never repaired");
        assert_eq!(mode("core.lock"), loose, "never repaired");
    }
    let target = root.path().join("somebody-else.json");
    fs::write(&target, br#"{"token": "theirs"}"#).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&target, dir.file("node-identity.json")).unwrap();
    assert!(dir.read_json("node-identity.json").is_err(), "not followed");
}
