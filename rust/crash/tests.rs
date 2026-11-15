use std::fs;
use std::path::Path;

use chrono::{Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::record::{frames, FORMATTED_MARKER};
use super::*;
use crate::storage::PrivateTemp;

fn at(days_ago: i64) -> String {
    (Utc::now() - Duration::days(days_ago)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn panic<'a>(literal: Option<&'a str>, line: u32, backtrace: &'a str) -> Panic<'a> {
    Panic {
        literal,
        file: "rust/server/x.rs",
        line,
        backtrace,
    }
}

fn record(line: u32) -> CrashRecord {
    CrashRecord::from_panic(
        Piece::Core,
        "1.2.3",
        "linux",
        "amd64",
        &panic(
            Some("boom"),
            line,
            "   0: a::b\n             at src/a.rs:1:2\n   1: c\n",
        ),
        &["info one".to_owned(), "error two".to_owned()],
        &[],
    )
}

struct Fixture {
    _root: PrivateTemp,
    data: std::path::PathBuf,
    store: CrashStore,
}

fn fixture() -> Fixture {
    let root = PrivateTemp::new();
    let data = root.path().join("core");
    fs::create_dir(&data).unwrap();
    let store = CrashStore::new(&data);
    Fixture {
        _root: root,
        data,
        store,
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn a_record_holds_the_closed_field_list_and_a_literal_message() {
    let value = serde_json::to_value(record(7)).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "arch",
            "backtrace",
            "count",
            "exception",
            "first_at",
            "id",
            "last_at",
            "location",
            "log_tail",
            "os",
            "piece",
            "signature",
            "version"
        ]
    );
    assert_eq!(
        value["exception"],
        json!({"type": "panic", "message": "boom"})
    );
    assert_eq!(
        value["location"],
        json!({"file": "rust/server/x.rs", "line": 7})
    );
    assert_eq!(value["backtrace"], json!(["a::b at src/a.rs:1:2", "c"]));
    assert_eq!(
        Uuid::parse_str(value["id"].as_str().unwrap())
            .unwrap()
            .get_version_num(),
        4
    );
}

#[test]
fn a_formatted_message_is_never_stored() {
    let canary = "canary-secret-7f3a";
    let stored = CrashRecord::from_panic(
        Piece::Core,
        "1.2.3",
        "linux",
        "amd64",
        &panic(None, 1, ""),
        &[],
        &[],
    );
    assert_eq!(stored.exception.message, FORMATTED_MARKER);
    assert!(!serde_json::to_string(&stored).unwrap().contains(canary));
}

#[test]
fn the_home_directory_is_shown_as_a_tilde() {
    let stored = CrashRecord::from_panic(
        Piece::Core,
        "1",
        "linux",
        "amd64",
        &Panic {
            literal: Some("x"),
            file: "/home/canary-user-7f3a/src/a.rs",
            line: 1,
            backtrace: "   0: f\n             at /home/canary-user-7f3a/b.rs:3:4\n",
        },
        &["opened /home/canary-user-7f3a/x".to_owned()],
        &["/home/canary-user-7f3a".to_owned()],
    );
    assert!(!serde_json::to_string(&stored)
        .unwrap()
        .contains("canary-user"));
}

#[test]
fn the_signature_groups_by_piece_version_location_and_top_frames() {
    let a = record(5);
    assert_eq!(a.signature, record(5).signature);
    assert_ne!(a.signature, record(6).signature, "location");
    let mut other = record(5);
    other.version = "1.2.4".to_owned();
    let other = CrashRecord::from_untrusted(&serde_json::to_value(&other).unwrap()).unwrap();
    assert_ne!(a.signature, other.signature, "version");
    assert_ne!(a.id, record(5).id, "ids are random");
}

#[test]
fn frames_pair_each_symbol_with_its_place() {
    assert_eq!(
        frames("stack backtrace:\n   0: one\n             at f.rs:1:1\n  10: two\n"),
        ["one at f.rs:1:1", "two"]
    );
}

#[test]
fn the_same_signature_keeps_one_record_with_a_count_and_the_times() {
    let f = fixture();
    let first = record(5);
    f.store.record(first.clone()).unwrap();
    let mut again = record(5);
    again.last_at = at(-1);
    f.store.record(again).unwrap();
    f.store.record(record(5)).unwrap();
    let listed = f.store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, first.id, "the first record stays");
    assert_eq!(listed[0].count, 3);
    assert_eq!(listed[0].first_at, first.first_at);
    assert!(listed[0].last_at > first.last_at);
    assert_eq!(names(&f.data.join("crashes")).len(), 1);
}

#[test]
fn at_most_twenty_records_are_kept_and_the_oldest_go_first() {
    let f = fixture();
    for line in 0..25u32 {
        let mut r = record(line);
        r.first_at = at(i64::from(25 - line) / 2);
        r.last_at = r.first_at.clone();
        f.store.record(r).unwrap();
    }
    let listed = f.store.list().unwrap();
    assert_eq!(listed.len(), MAX_RECORDS);
    let lines: Vec<u32> = listed.iter().map(|r| r.location.line).collect();
    assert!(lines.iter().all(|line| *line >= 5), "{lines:?}");
}

#[test]
fn a_record_older_than_thirty_days_is_deleted() {
    let f = fixture();
    let mut old = record(1);
    old.first_at = at(31);
    old.last_at = at(31);
    f.store.record(old).unwrap();
    f.store.record(record(2)).unwrap();
    let listed = f.store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].location.line, 2);
    assert_eq!(names(&f.data.join("crashes")).len(), 1);
}

#[test]
fn the_largest_possible_records_fit_the_caps() {
    let f = fixture();
    for line in 0..MAX_RECORDS as u32 {
        let mut r = record(line);
        r.log_tail = vec!["x".repeat(600); TAIL_LINES];
        r.backtrace = vec!["y".repeat(240); MAX_FRAMES];
        r.last_at = at(i64::from(line) % 3);
        f.store.record(r).unwrap();
    }
    let bytes: u64 = fs::read_dir(f.data.join("crashes"))
        .unwrap()
        .flatten()
        .map(|entry| entry.metadata().unwrap().len())
        .sum();
    assert!(bytes <= MAX_BYTES, "{bytes}");
    assert_eq!(f.store.list().unwrap().len(), MAX_RECORDS);
}

#[test]
fn a_record_is_cut_to_the_tail_and_frame_limits() {
    let mut big = record(1);
    big.log_tail = (0..500).map(|n| format!("line {n}")).collect();
    big.backtrace = (0..500).map(|n| format!("frame {n}")).collect();
    let read = CrashRecord::from_untrusted(&serde_json::to_value(&big).unwrap()).unwrap();
    assert_eq!(read.log_tail.len(), TAIL_LINES);
    assert_eq!(read.log_tail.last().unwrap(), "line 499", "the last lines");
    assert_eq!(read.backtrace.len(), MAX_FRAMES);
}

#[test]
fn corrupt_partial_oversized_and_foreign_files_are_removed_or_left_and_never_panic() {
    let f = fixture();
    f.store.record(record(1)).unwrap();
    let dir = f.data.join("crashes");
    let id = Uuid::new_v4().to_string();
    fs::write(dir.join(format!("crash-{id}.json")), b"{\"id\": \"trunc").unwrap();
    let other = Uuid::new_v4().to_string();
    fs::write(
        dir.join(format!("crash-{other}.json")),
        [0u8, 159, 146, 150],
    )
    .unwrap();
    let huge = Uuid::new_v4().to_string();
    fs::write(
        dir.join(format!("crash-{huge}.json")),
        vec![b' '; MAX_RECORD_BYTES as usize + 10],
    )
    .unwrap();
    // A valid record under the wrong name is not read as that name's.
    let misnamed = serde_json::to_vec(&record(9)).unwrap();
    fs::write(dir.join(format!("crash-{}.json", Uuid::new_v4())), misnamed).unwrap();
    fs::write(dir.join("notes.txt"), b"kept").unwrap();
    fs::create_dir(dir.join(format!("crash-{}.json", Uuid::new_v4()))).unwrap();
    let listed = f.store.list().unwrap();
    assert_eq!(listed.len(), 1);
    let left = names(&dir);
    assert!(left.contains(&"notes.txt".to_owned()));
    assert!(
        left.iter()
            .filter(|name| name.starts_with("crash-"))
            .count()
            <= 2,
        "{left:?}"
    );
}

#[test]
fn acknowledging_deletes_the_record_and_only_by_its_id() {
    let f = fixture();
    let kept = record(1);
    f.store.record(kept.clone()).unwrap();
    f.store.record(record(2)).unwrap();
    for bad in [
        "../core/node.json",
        "x",
        "",
        "crash-1",
        &kept.id.to_uppercase(),
    ] {
        assert!(!f.store.acknowledge(bad).unwrap(), "{bad}");
    }
    assert!(f.store.acknowledge(&kept.id).unwrap());
    assert!(!f.store.acknowledge(&kept.id).unwrap(), "already gone");
    assert_eq!(f.store.list().unwrap().len(), 1);
}

#[test]
fn the_connectors_spool_is_taken_in_validated_and_emptied() {
    let f = fixture();
    let inbox = f.data.parent().unwrap().join(INBOX);
    <crate::platform::fs::Files as crate::platform::fs::Fs>::create_private_dir(&inbox).unwrap();
    let mut mine = serde_json::to_value(record(3)).unwrap();
    mine["piece"] = json!("connector");
    mine["extra"] = json!("not in the closed list");
    mine["signature"] = json!("forged");
    let id = mine["id"].as_str().unwrap().to_owned();
    <crate::platform::fs::Files as crate::platform::fs::Fs>::create_private(
        &inbox.join(format!("crash-{id}.json")),
        mine.to_string().as_bytes(),
    )
    .unwrap();
    fs::write(inbox.join("crash-bad.json"), b"nope").unwrap();
    let mut listed = f.store.list().unwrap();
    assert_eq!(listed.len(), 1);
    let taken = listed.remove(0);
    assert_eq!(taken.piece, Piece::Connector);
    assert_ne!(taken.signature, "forged", "made again, not believed");
    let value: Value = serde_json::to_value(&taken).unwrap();
    assert!(value.get("extra").is_none());
    assert!(names(&inbox).is_empty(), "{:?}", names(&inbox));
}

#[test]
fn a_record_with_a_bad_field_is_not_taken() {
    let base = serde_json::to_value(record(1)).unwrap();
    for (field, value) in [
        ("id", json!("../../x")),
        ("piece", json!("engine")),
        ("version", json!("1 2")),
        ("os", json!("")),
        ("last_at", json!("yesterday")),
    ] {
        let mut bad = base.clone();
        bad[field] = value;
        assert!(CrashRecord::from_untrusted(&bad).is_none(), "{field}");
    }
    let mut dirty = base;
    dirty["exception"]["message"] = json!("a\u{1b}[31mred\nline");
    let clean = CrashRecord::from_untrusted(&dirty).unwrap();
    assert!(!clean.exception.message.chars().any(char::is_control));
}

#[test]
fn the_log_tail_is_the_last_lines_without_backtrace_events() {
    let f = fixture();
    let log = f.data.parent().unwrap().join("core.log");
    let mut text = String::new();
    for n in 0..80 {
        text.push_str(&format!("INFO sidevoice_core: line {n}\n"));
        if n == 79 {
            text.push_str("ERROR crash_backtrace: exception.stacktrace=0: x\n");
        }
    }
    <crate::platform::fs::Files as crate::platform::fs::Fs>::write_private(&log, text.as_bytes())
        .unwrap();
    let tail = store::log_tail(&log);
    assert_eq!(tail.len(), TAIL_LINES);
    assert!(tail.last().unwrap().ends_with("line 79"));
    assert!(store::log_tail(&f.data.join("missing.log")).is_empty());
}
