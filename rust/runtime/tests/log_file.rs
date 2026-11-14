//! The core's own log file end to end: private, home shown as `~`, rotated at 5 MB keeping two, and every line judged
//! again before it is written.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

use super::support::args;
use crate::runtime::event_log::LogFile;
use crate::runtime::Config;
use crate::storage::{is_private_file, PrivateTemp};

fn open(log: PathBuf) -> io::Result<LogFile> {
    LogFile::open(log, None)
}

/// One line, as the subscriber's formatter writes an event.
fn emit(log: &LogFile, line: &str) -> io::Result<()> {
    log.make_writer().write_all(format!("{line}\n").as_bytes())
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
    let writer = open(log.clone()).unwrap();
    assert!(is_private_file(&log), "created private, at once");
    for generation in 0..4 {
        let mut file = OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(&vec![b'x'; 5_000_000]).unwrap();
        drop(file);
        emit(&writer, &format!("generation {generation}")).unwrap();
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
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "generation 3\n",
        "the newest line starts a new file"
    );
}

#[test]
fn the_home_directory_is_shown_as_a_tilde() {
    let root = PrivateTemp::new();
    let log = root.path().join("core.log");
    let writer = LogFile::open(log.clone(), Some(Path::new("/home/canary-user-7f3a"))).unwrap();
    emit(
        &writer,
        r#"at /home/canary-user-7f3a/x and "/home/canary-user-7f3a/y""#,
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), "at ~/x and \"~/y\"\n");
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
    assert!(open(log).is_err(), "refused when the log is opened");
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
}

// Unix only: the unsafe fixtures are built with `chmod`; the rule is the storage's (`PrivateDir`, `Files`), tested on
// every OS beside it.
#[cfg(unix)]
#[test]
fn a_log_where_others_can_enter_or_read_is_refused_not_repaired() {
    use crate::platform::fs::{Files, Fs};
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("logs");
    let log = parent.join("core.log");
    let writer = open(log.clone()).unwrap();
    assert!(
        Files::ensure_private_dir(&parent).is_ok(),
        "a parent it made is private"
    );

    fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(emit(&writer, "refused").is_err(), "a later line is refused");
    assert!(open(log.clone()).is_err());
    assert_eq!(mode(&log), 0o644, "not repaired");
    fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();

    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(emit(&writer, "refused").is_err(), "a later line is refused");
    assert!(open(log).is_err());
    assert_eq!(mode(&parent), 0o755, "not repaired");
    assert_eq!(fs::read(parent.join("core.log")).unwrap(), b"");
}

// Unix only: the unsafe fixtures are built with `chmod` and a symlink; the refusal itself is the interface's
// (`Files::open_private`, `Files::check_private`), tested on every OS in `sidevoice-platform`.
#[cfg(unix)]
#[test]
fn a_full_log_that_is_unsafe_is_refused_before_it_is_rotated() {
    use std::os::unix::fs::PermissionsExt;
    let root = PrivateTemp::new();
    let log = root.path().join("core.log");
    fs::write(&log, vec![b'x'; 5_000_000]).unwrap();
    fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(open(log.clone()).is_err());
    assert_eq!(fs::metadata(&log).unwrap().len(), 5_000_000, "not rotated");
    assert!(!root.path().join("core.log.1").exists());

    fs::remove_file(&log).unwrap();
    let target = root.path().join("somebody-else");
    fs::write(&target, vec![b'x'; 5_000_000]).unwrap();
    std::os::unix::fs::symlink(&target, &log).unwrap();
    assert!(open(log.clone()).is_err());
    assert!(
        fs::symlink_metadata(&log).unwrap().is_symlink(),
        "left where it was"
    );
    assert!(!root.path().join("core.log.1").exists());
}
