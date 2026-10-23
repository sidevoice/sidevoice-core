//! The core's own log end to end: private, JSON lines naming the launch, rotated at 5 MB keeping two.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use serde_json::Value;
use uuid::Uuid;

use super::support::args;
use crate::runtime::event_log::log_event;
use crate::runtime::Config;
use crate::storage::{is_private_file, PrivateTemp};

fn config(log_file: PathBuf) -> Config {
    let root = log_file.parent().unwrap().to_owned();
    Config {
        data_dir: root.join("core"),
        endpoint: <crate::platform::ipc::Endpoint as crate::platform::ipc::Address>::new(
            &root.join("core"),
            "local",
        ),
        ready_file: root.join("core/core.json"),
        host: "127.0.0.1".into(),
        port: 0,
        launch_id: "logged".into(),
        log_file,
        idle_exit: 0.0,
    }
}

#[test]
fn a_launch_without_an_id_makes_one_up() {
    let parsed = Config::from_args(args(&["--data-dir", "relative/x"])).unwrap();
    assert_eq!(
        Uuid::parse_str(&parsed.launch_id).unwrap().to_string(),
        parsed.launch_id
    );
}

#[test]
fn the_log_rotates_at_five_megabytes_and_keeps_two() {
    let root = PrivateTemp::new();
    let log = root.path().join("core.log");
    let config = config(log.clone());
    log_event(&config, "runtime.log_start", None).unwrap();
    assert!(is_private_file(&log));
    for generation in 0..4 {
        let mut file = OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(&vec![b'x'; 5_000_000]).unwrap();
        drop(file);
        log_event(
            &config,
            "runtime.log_ready",
            Some(&format!("g{generation}")),
        )
        .unwrap();
    }
    let mut files: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files, ["core.log", "core.log.1", "core.log.2"], "two kept");
    for name in &files {
        let path = root.path().join(name);
        let len = fs::metadata(&path).unwrap().len();
        assert!(len <= 5_000_000 + 2_000, "{name}: {len}");
        assert!(is_private_file(&path), "{name}");
    }
    let current = fs::read_to_string(&log).unwrap();
    assert_eq!(
        current.lines().count(),
        1,
        "the newest line starts a new file"
    );
    let line: Value = serde_json::from_str(current.trim()).unwrap();
    assert_eq!(line["key"], "runtime.log_ready");
    assert_eq!(line["launch_id"], "logged");
    assert!(line["message"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));
    assert!(line["at"].as_str().unwrap().ends_with('Z'), "UTC");
}

// Unix only: a symlink needs no privilege there; the refusal of a link in the log's place is tested on every OS against the
// interface (`sidevoice-platform`, fs tests).
#[cfg(unix)]
#[test]
fn a_log_that_is_a_link_is_not_written_through() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("somebody-else");
    fs::write(&target, b"untouched").unwrap();
    let log = root.path().join("core.log");
    std::os::unix::fs::symlink(&target, &log).unwrap();
    assert!(log_event(&config(log), "runtime.log_start", None).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
}

// Unix only: the unsafe fixtures are built with `chmod`; the rule is the storage's (`PrivateDir`, `Files`), tested on
// every OS beside it.
#[cfg(unix)]
#[test]
fn a_log_where_others_can_enter_or_read_is_refused_not_repaired() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("logs");
    let log = parent.join("core.log");
    log_event(&config(log.clone()), "runtime.log_start", None).unwrap();
    assert!(is_private_file(&parent), "a parent it made is private");

    fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(log_event(&config(log.clone()), "runtime.log_ready", None).is_err());
    assert_eq!(mode(&log), 0o644, "not repaired");
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();

    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(log_event(&config(log), "runtime.log_ready", None).is_err());
    assert_eq!(mode(&parent), 0o755, "not repaired");
}
