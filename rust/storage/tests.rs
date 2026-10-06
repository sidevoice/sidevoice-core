//! The private-directory and private-file rules, and the process lock.

use super::*;

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_directory_it_creates_is_this_user_s_alone() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("nested").join("core");
    PrivateDir::open(&path).unwrap();
    assert_eq!(mode(&path), 0o700);
}

#[test]
fn only_a_directory_this_user_alone_may_enter_is_safe() {
    let root = tempfile::tempdir().unwrap();
    let safe = root.path().join("core");
    fs::DirBuilder::new().mode(0o700).create(&safe).unwrap();
    PrivateDir::open(&safe).unwrap();
    for loose in [0o750, 0o705, 0o770, 0o777, 0o701] {
        fs::set_permissions(&safe, Permissions::from_mode(loose)).unwrap();
        let refused = PrivateDir::open(&safe).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{loose:o}");
        assert_eq!(mode(&safe), loose, "a refused directory is left as it was");
    }
    fs::set_permissions(&safe, Permissions::from_mode(0o700)).unwrap();
    let file = root.path().join("file");
    fs::write(&file, b"").unwrap();
    assert!(
        PrivateDir::open(&file).is_err(),
        "a file is not a directory"
    );
}

#[test]
fn a_link_to_a_directory_is_not_the_directory() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("elsewhere");
    fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
    let link = root.path().join("core");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(PrivateDir::open(&link).is_err());
    assert!(PrivateDir::open_for_report(&link).is_err());
}

#[test]
#[ignore = "known gap: a trailing `/`, `/.` or `//` on a symlinked data directory is followed (lstat resolves it)"]
fn a_link_is_refused_however_it_is_spelled() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("elsewhere");
    fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
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
    if rustix::process::getuid().is_root() {
        return; // root owns `/`; unprivileged, `/` stands for another user's directory.
    }
    let refused = PrivateDir::open("/").unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
    assert!(PrivateDir::open_for_report("/").is_err());
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
    assert_eq!(mode(&dir.file("integrations.json")), 0o600);
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
        assert_eq!(
            mode(staged.path()),
            0o600,
            "private before it is renamed in"
        );
    }
}

#[test]
fn a_write_that_fails_leaves_the_old_file_and_no_temporary() {
    if rustix::process::getuid().is_root() {
        return; // root writes into a read-only directory.
    }
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    dir.write_json(
        "integrations.json",
        &serde_json::json!({"openai": "sk-working-0000"}),
    )
    .unwrap();
    fs::set_permissions(dir.path(), Permissions::from_mode(0o500)).unwrap();
    let failed = dir.write_json(
        "integrations.json",
        &serde_json::json!({"openai": "sk-never-1111"}),
    );
    fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
    assert!(failed.is_err());
    assert_eq!(
        dir.read_json("integrations.json").unwrap().unwrap(),
        serde_json::json!({"openai": "sk-working-0000"})
    );
    assert_eq!(names(dir.path()), ["integrations.json"]);

    // A rename that fails (the name is taken by a directory) removes its staging file too.
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
    assert_eq!(mode(&dir.file("node-identity.json")), 0o600);
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
    assert_eq!(mode(&dir.file("core.lock")), 0o600);
    drop(held);
    assert!(dir.lock().unwrap().is_some(), "released with its holder");
}

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
