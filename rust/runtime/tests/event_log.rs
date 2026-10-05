use std::fs;

use crate::runtime::event_log::rotate_if_full;

#[test]
fn a_log_below_the_limit_is_left_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("core.log");
    fs::write(&log, b"line\n").unwrap();
    rotate_if_full(&log).unwrap();
    assert_eq!(fs::read(&log).unwrap(), b"line\n");
    assert!(!dir.path().join("core.log.1").exists());
}

#[test]
fn a_full_log_shifts_two_generations_and_drops_the_oldest() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("core.log");
    let first = dir.path().join("core.log.1");
    let second = dir.path().join("core.log.2");
    fs::write(&log, vec![b'x'; 5_000_000]).unwrap();
    fs::write(&first, b"first").unwrap();
    fs::write(&second, b"second").unwrap();
    rotate_if_full(&log).unwrap();
    assert!(!log.exists());
    assert_eq!(fs::metadata(&first).unwrap().len(), 5_000_000);
    assert_eq!(fs::read(&second).unwrap(), b"first");
}
