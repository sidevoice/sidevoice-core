use std::fs;
use std::path::Path;

use chrono::{Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use super::record::{frames, mask_home, mask_homes, FORMATTED_MARKER};
use super::*;
use crate::storage::PrivateTemp;

fn at(days_ago: i64) -> String {
    (Utc::now() - Duration::days(days_ago)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A line of the core's own log, at error.
fn ours(n: impl std::fmt::Display) -> String {
    format!("2026-11-15T10:00:00.123456Z ERROR sidevoice_core::x: line {n}")
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
        &[ours(1), ours(2)],
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
    let store = CrashStore::new(&data, Vec::new());
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
    let other = CrashRecord::from_untrusted(&serde_json::to_value(&other).unwrap(), &[]).unwrap();
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
        r.log_tail = vec![format!("{} {}", ours(0), "x".repeat(600)); TAIL_LINES];
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
    big.log_tail = (0..500).map(ours).collect();
    big.backtrace = (0..500).map(|n| format!("frame {n}")).collect();
    let read = CrashRecord::from_untrusted(&serde_json::to_value(&big).unwrap(), &[]).unwrap();
    assert_eq!(read.log_tail.len(), TAIL_LINES);
    assert_eq!(read.log_tail.last().unwrap(), &ours(499), "the last lines");
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
        assert!(f.store.acknowledge(bad).unwrap().is_none(), "{bad}");
    }
    assert_eq!(
        f.store.acknowledge(&kept.id).unwrap(),
        Some(kept.signature.clone())
    );
    assert!(
        f.store.acknowledge(&kept.id).unwrap().is_none(),
        "already gone"
    );
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
        assert!(CrashRecord::from_untrusted(&bad, &[]).is_none(), "{field}");
    }
    let mut dirty = base;
    dirty["exception"]["message"] = json!("a\u{1b}[31mred\nline");
    let clean = CrashRecord::from_untrusted(&dirty, &[]).unwrap();
    assert!(!clean.exception.message.chars().any(char::is_control));
}

#[test]
fn the_log_tail_keeps_only_our_warnings_and_errors_and_the_last_twenty() {
    let f = fixture();
    let log = f.data.parent().unwrap().join("core.log");
    let mut text = String::new();
    for n in 0..80 {
        text.push_str(&format!("{}\n", ours(n)));
    }
    text.push_str("2026-11-15T10:00:01.000000Z ERROR crash_backtrace: exception.stacktrace=0: x\n");
    text.push_str(
        "2026-11-15T10:00:01.000000Z  WARN hyper_util::client: GET https://x/?token=canary\n",
    );
    text.push_str("2026-11-15T10:00:01.000000Z DEBUG sidevoice_core::x: a debug line\n");
    text.push_str("2026-11-15T10:00:01.000000Z  INFO sidevoice_core::x: an info line\n");
    text.push_str("raw stderr of a service ?token=canary\n");
    text.push_str("2026-11-15T10:00:01.000000Z  WARN sidevoice_corevoice: a lookalike target\n");
    <crate::platform::fs::Files as crate::platform::fs::Fs>::write_private(&log, text.as_bytes())
        .unwrap();
    let tail = store::log_tail(&log);
    assert!(!tail.iter().any(|line| line.contains("crash_backtrace")));
    assert!(store::log_tail(&f.data.join("missing.log")).is_empty());
    let stored = CrashRecord::from_panic(
        Piece::Core,
        "1",
        "linux",
        "amd64",
        &panic(Some("x"), 1, ""),
        &tail,
        &[],
    );
    assert_eq!(stored.log_tail.len(), TAIL_LINES);
    assert_eq!(stored.log_tail.last().unwrap(), &ours(79));
    let all = serde_json::to_string(&stored).unwrap();
    for dropped in [
        "canary",
        "debug line",
        "info line",
        "lookalike",
        "hyper_util",
    ] {
        assert!(!all.contains(dropped), "{dropped}");
    }
}

#[test]
fn a_tail_line_is_cut_stripped_of_format_characters_and_never_forged_by_a_piece() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([
        format!("{} {}", ours(1), "z".repeat(900)),
        format!("{} \u{202e}reversed\u{200b}", ours(2)),
        "2026-11-15T10:00:00.000Z ERROR sidevoice_connector: a connector line in a core record",
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.log_tail.len(), 2);
    assert_eq!(read.log_tail[0].chars().count(), 600);
    assert!(read.log_tail[1].ends_with("reversed"));
    assert!(!read.log_tail[1].contains('\u{202e}'));
}

#[test]
fn texts_that_are_not_what_we_write_are_replaced_or_dropped() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["exception"]["message"] = json!("caf\u{e9} \u{202e}x");
    raw["location"]["file"] = json!("/etc/passwd");
    raw["backtrace"] = json!([
        "good::frame at src/a.rs:1:2",
        "bad frame ?token=canary at src/a.rs:1",
        "frame at src/a b.rs:1",
        "frame at src/a.rs:x",
        "plain::frame",
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.exception.message, FORMATTED_MARKER);
    assert_eq!(read.location.file, "unknown");
    assert_eq!(
        read.backtrace,
        ["good::frame at src/a.rs:1:2", "plain::frame"]
    );
    for file in ["../a.rs", "a b.rs", "C:/a.rs", "~/a.rs"] {
        raw["location"]["file"] = json!(file);
        assert_eq!(
            CrashRecord::from_untrusted(&raw, &[])
                .unwrap()
                .location
                .file,
            "unknown",
            "{file}"
        );
    }
    raw["location"]["file"] = json!("rust\\server\\x.rs");
    assert_eq!(
        CrashRecord::from_untrusted(&raw, &[])
            .unwrap()
            .location
            .file,
        "rust/server/x.rs"
    );
}

#[test]
fn times_are_made_utc_and_ordered_and_the_home_is_hidden_again() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["first_at"] = json!("2026-11-15T12:00:00.000+02:00");
    raw["last_at"] = json!("2026-11-15T09:00:00.000Z");
    raw["log_tail"] = json!([format!("{} at /home/canary-user-7f3a/x", ours(1))]);
    let read = CrashRecord::from_untrusted(&raw, &["/home/canary-user-7f3a".to_owned()]).unwrap();
    assert_eq!(read.first_at, "2026-11-15T09:00:00.000Z");
    assert_eq!(read.last_at, "2026-11-15T10:00:00.000Z");
    assert!(!serde_json::to_string(&read)
        .unwrap()
        .contains("canary-user"));
    assert!(read.log_tail[0].ends_with("at ~/x"));
}

fn spool(f: &Fixture) -> std::path::PathBuf {
    let inbox = f.data.parent().unwrap().join(INBOX);
    <crate::platform::fs::Files as crate::platform::fs::Fs>::ensure_private_dir(&inbox).unwrap();
    inbox
}

fn drop_in(inbox: &Path, record: &CrashRecord) {
    <crate::platform::fs::Files as crate::platform::fs::Fs>::write_private(
        &inbox.join(format!("crash-{}.json", record.id)),
        serde_json::to_vec(record).unwrap().as_slice(),
    )
    .unwrap();
}

#[test]
fn records_dated_in_the_future_do_not_push_a_real_one_out() {
    let f = fixture();
    let inbox = spool(&f);
    let real = record(1);
    f.store.record(real.clone()).unwrap();
    for line in 100..120 {
        let mut planted = record(line);
        planted.piece = Piece::Connector;
        planted.first_at = "9999-01-01T00:00:00.000Z".to_owned();
        planted.last_at = planted.first_at.clone();
        drop_in(&inbox, &planted);
    }
    let listed = f.store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, real.id);
    assert!(names(&inbox).is_empty(), "{:?}", names(&inbox));
}

#[test]
fn a_symlinked_spool_is_not_followed() {
    let f = fixture();
    let root = f.data.parent().unwrap();
    let elsewhere = root.join("elsewhere");
    <crate::platform::fs::Files as crate::platform::fs::Fs>::ensure_private_dir(&elsewhere)
        .unwrap();
    drop_in(&elsewhere, &record(1));
    #[cfg(unix)]
    std::os::unix::fs::symlink(&elsewhere, root.join(INBOX)).unwrap();
    #[cfg(unix)]
    {
        assert!(f.store.list().unwrap().is_empty());
        assert_eq!(names(&elsewhere).len(), 1, "nothing was taken from it");
    }
}

#[test]
fn a_file_the_store_refuses_for_good_is_deleted_not_retried() {
    let f = fixture();
    let inbox = spool(&f);
    let one = record(1);
    f.store.record(one.clone()).unwrap();
    // The same id again: the store never replaces a file.
    let mut again = record(2);
    again.id = one.id.clone();
    drop_in(&inbox, &again);
    f.store.list().unwrap();
    assert!(names(&inbox).is_empty(), "{:?}", names(&inbox));
}

#[test]
fn staging_files_are_left_alone_until_they_are_old() {
    let f = fixture();
    let inbox = spool(&f);
    let staged = inbox.join(format!(".crash-{}.json.AbCdEf", Uuid::new_v4()));
    fs::write(&staged, b"{\"half").unwrap();
    f.store.list().unwrap();
    assert!(staged.exists(), "a writer may still be at it");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    fs::File::options()
        .write(true)
        .open(&staged)
        .unwrap()
        .set_modified(old)
        .unwrap();
    f.store.list().unwrap();
    assert!(!staged.exists());
}

#[test]
fn format_characters_are_stripped_before_the_home_is_masked() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([format!("{} at /data/al\u{200b}ice/x", ours(1))]);
    let read = CrashRecord::from_untrusted(&raw, &["/data/alice".to_owned()]).unwrap();
    assert!(read.log_tail[0].ends_with("at ~/x"), "{:?}", read.log_tail);
}

#[test]
fn a_date_past_year_9999_is_not_taken_and_the_future_is_compared_as_time() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["first_at"] = json!("9999-12-31T23:59:59-23:59");
    raw["last_at"] = json!("9999-12-31T23:59:59-23:59");
    assert!(CrashRecord::from_untrusted(&raw, &[]).is_none());
    let mut future = record(1);
    future.first_at = "+10000-01-01T00:00:00.000Z".to_owned();
    assert!(future.is_dated_in_the_future());
    assert!(!record(1).is_dated_in_the_future());
}

#[test]
fn the_home_is_matched_without_case_and_any_users_root_is_masked() {
    assert_eq!(
        mask_home(
            "D:\\Profiles\\\u{c9}LISE\\x",
            "d:\\profiles\\\u{e9}lise",
            true
        ),
        "~\\x"
    );
    assert_eq!(
        mask_homes("C:\\USERS\\ALICE~1\\x /Users/Bob/y /home/carol z", &[]),
        "~\\x ~/y ~ z"
    );
    assert_eq!(
        mask_homes("/usr/home/x /homes/y", &[]),
        "/usr/home/x /homes/y"
    );
}

#[test]
fn a_warning_inside_a_span_is_kept_and_a_dependency_inside_a_span_is_dropped() {
    let t = "2026-11-15T10:00:00.123456Z";
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([
        format!("{t}  WARN binding{{id=\"a: b}}\" n=3}}: sidevoice_core::room: kept one"),
        format!("{t}  WARN outer{{a=1}}: inner: sidevoice_core::room: kept two"),
        format!("{t}  WARN binding{{id=1}}: hyper_util::client: dropped one"),
        format!("{t}  WARN binding{{id=1}}: sidevoice_corevoice: dropped two"),
        format!("{t}  WARN binding{{id=1}} sidevoice_core::room: dropped three"),
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.log_tail.len(), 2, "{:?}", read.log_tail);
    assert!(read.log_tail[1].contains("kept two"));
}
