use std::fs;
use std::path::Path;

use chrono::{Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::record::{mask_home, mask_homes, FORMATTED_MARKER};
use super::report::{deliver, drain, event, pass, Undelivered};
use super::*;
use crate::platform::fs::{Files, Fs};
use crate::storage::PrivateTemp;

const ID: &str = "7b2f0b00-1c3e-4d5a-8f6b-0123456789ab";

fn at(days_ago: i64) -> String {
    (Utc::now() - Duration::days(days_ago)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A line of the core's own tail, at error, as `tail` renders it.
fn ours(n: impl std::fmt::Display) -> String {
    format!("-1.000s ERROR sidevoice_core::x: line {n}")
}

fn frame(function: &str, addr: u64) -> Frame {
    Frame {
        function: Some(function.to_owned()),
        file: Some("a.rs".to_owned()),
        line: Some(1),
        addr,
    }
}

fn image(addr: u64, size: u64) -> Image {
    Image {
        name: "sidevoice-core".to_owned(),
        addr,
        size,
        vmaddr: 0x40_0000,
        id: ID.to_owned(),
        code_id: Some("7b2f0b001c3e4d5a8f6b".to_owned()),
    }
}

fn panic<'a>(literal: Option<&'a str>, line: u32) -> Panic<'a> {
    Panic {
        version: "1.2.3",
        os: "linux",
        arch: "amd64",
        at: Utc::now(),
        literal,
        formatted_len: 0,
        file: "rust/server/x.rs",
        line,
        frames: vec![frame("a::b", 0x1010), frame("c", 0x1020)],
        images: vec![image(0x1000, 0x1000)],
        tail: vec![ours(1), ours(2)],
    }
}

fn record(line: u32) -> CrashRecord {
    CrashRecord::from_panic(panic(Some("boom"), line), &[])
}

struct Fixture {
    _root: PrivateTemp,
    data: std::path::PathBuf,
    store: CrashStore,
}

fn fixture() -> Fixture {
    let root = PrivateTemp::new();
    let data = root.path().join("core");
    Files::create_private_dir(&data).unwrap();
    let store = CrashStore::new(&data, Vec::new());
    Fixture {
        _root: root,
        data,
        store,
    }
}

fn context(f: &Fixture) -> Context {
    Context {
        data: f.data.clone(),
        store: CrashStore::new(&f.data, Vec::new()),
        homes: Vec::new(),
        os: "linux",
        arch: "amd64",
        images: Vec::new(),
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

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

// The record.

#[test]
fn a_record_holds_the_closed_field_list_and_a_literal_message() {
    let value = serde_json::to_value(record(7)).unwrap();
    assert_eq!(
        keys(&value),
        [
            "arch",
            "backtrace",
            "count",
            "exception",
            "first_at",
            "id",
            "images",
            "last_at",
            "location",
            "log_tail",
            "os",
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
    assert_eq!(
        value["backtrace"],
        json!([
            {"function": "a::b", "file": "a.rs", "line": 1, "addr": 0x1010},
            {"function": "c", "file": "a.rs", "line": 1, "addr": 0x1020}
        ])
    );
    assert_eq!(value["images"][0]["name"], "sidevoice-core");
    assert_eq!(
        Uuid::parse_str(value["id"].as_str().unwrap())
            .unwrap()
            .get_version_num(),
        4
    );
}

#[test]
fn a_formatted_message_is_never_stored_only_its_length() {
    let canary = "canary-secret-7f3a";
    let mut formatted = panic(None, 1);
    formatted.formatted_len = canary.len();
    let stored = CrashRecord::from_panic(formatted, &[]);
    assert_eq!(stored.exception.message, FORMATTED_MARKER);
    assert_eq!(stored.exception.len, Some(canary.len() as u32));
    assert!(!serde_json::to_string(&stored).unwrap().contains(canary));
    // A literal has no length to say.
    let mut literal = panic(Some("boom"), 1);
    literal.formatted_len = 9;
    assert_eq!(CrashRecord::from_panic(literal, &[]).exception.len, None);
}

#[test]
fn the_home_directory_is_shown_as_a_tilde_and_no_file_keeps_a_directory() {
    let mut raw = panic(Some("x"), 1);
    raw.file = "/home/canary-user-7f3a/src/a.rs";
    raw.frames = vec![Frame {
        function: Some("f".to_owned()),
        file: Some("/home/canary-user-7f3a/b.rs".to_owned()),
        line: Some(3),
        addr: 1,
    }];
    raw.images = vec![Image {
        name: "/home/canary-user-7f3a/.local/bin/sidevoice-core".to_owned(),
        ..image(0, 1)
    }];
    raw.tail = vec![format!("{} opened /home/canary-user-7f3a/x", ours(1))];
    let stored = CrashRecord::from_panic(raw, &["/home/canary-user-7f3a".to_owned()]);
    let json = serde_json::to_string(&stored).unwrap();
    assert!(!json.contains("canary-user"), "{json}");
    assert_eq!(stored.backtrace[0].file.as_deref(), Some("b.rs"));
    assert_eq!(stored.images[0].name, "sidevoice-core");
    assert_eq!(stored.location.file, "unknown");
}

#[test]
fn the_signature_groups_by_version_location_and_top_frames_and_not_by_address() {
    let a = record(5);
    assert_eq!(a.signature, record(5).signature);
    assert_ne!(a.signature, record(6).signature, "location");
    let mut other = record(5);
    other.version = "1.2.4".to_owned();
    let other = CrashRecord::from_untrusted(&serde_json::to_value(&other).unwrap(), &[]).unwrap();
    assert_ne!(a.signature, other.signature, "version");
    let mut moved = panic(Some("boom"), 5);
    moved.frames = vec![frame("a::b", 0x9010), frame("c", 0x9020)];
    assert_eq!(
        a.signature,
        CrashRecord::from_panic(moved, &[]).signature,
        "a new address space is the same crash"
    );
    let mut deeper = panic(Some("boom"), 5);
    deeper.frames = vec![frame("a::other", 0x1010)];
    assert_ne!(
        a.signature,
        CrashRecord::from_panic(deeper, &[]).signature,
        "frames"
    );
    assert_ne!(a.id, record(5).id, "ids are random");
}

#[test]
fn a_record_is_cut_to_the_tail_frame_and_image_limits() {
    let mut big = serde_json::to_value(record(1)).unwrap();
    big["log_tail"] = json!((0..500).map(ours).collect::<Vec<_>>());
    big["backtrace"] = json!((0..500)
        .map(|n| json!({"function": format!("frame{n}"), "addr": n}))
        .collect::<Vec<_>>());
    big["images"] = json!((0..500)
        .map(|n| json!({"name": "x", "addr": n, "size": 1, "vmaddr": 0, "id": ID}))
        .collect::<Vec<_>>());
    let read = CrashRecord::from_untrusted(&big, &[]).unwrap();
    assert_eq!(read.log_tail.len(), TAIL_LINES);
    assert!(
        read.log_tail.last().unwrap().ends_with("line 499"),
        "the last lines"
    );
    assert_eq!(read.backtrace.len(), MAX_FRAMES);
    assert_eq!(read.images.len(), MAX_IMAGES);
}

#[test]
fn a_record_with_a_bad_field_is_not_taken() {
    let base = serde_json::to_value(record(1)).unwrap();
    for (field, value) in [
        ("id", json!("../../x")),
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
fn texts_that_are_not_what_we_write_are_replaced_or_dropped() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["exception"]["message"] = json!("caf\u{e9} \u{202e}x");
    raw["location"]["file"] = json!("/etc/passwd");
    raw["backtrace"] = json!([
        {"function": "good::frame", "file": "a.rs", "line": 2, "addr": 1},
        {"function": "bad frame ?token=canary", "file": "/home/x/a b.rs", "addr": 2},
        {"file": "../../etc/passwd", "addr": 3},
        {"function": "no::address"},
        "not a frame",
    ]);
    raw["images"] = json!([
        {"name": "/opt/sidevoice/bin/sidevoice-core", "addr": 1, "size": 2, "vmaddr": 0, "id": ID},
        {"name": "a b", "addr": 1, "size": 2, "vmaddr": 0, "id": ID},
        {"name": "ok", "addr": 1, "size": 2, "vmaddr": 0, "id": "not a debug id"},
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.exception.message, FORMATTED_MARKER);
    assert_eq!(read.location.file, "unknown");
    assert_eq!(
        read.backtrace,
        [
            Frame {
                function: Some("good::frame".to_owned()),
                file: Some("a.rs".to_owned()),
                line: Some(2),
                addr: 1
            },
            Frame {
                function: None,
                file: None,
                line: None,
                addr: 2
            },
            Frame {
                function: None,
                file: Some("passwd".to_owned()),
                line: None,
                addr: 3
            },
        ]
    );
    assert_eq!(read.images.len(), 1);
    assert_eq!(read.images[0].name, "sidevoice-core");
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
fn a_tail_line_is_cut_stripped_of_format_characters_and_only_ours() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([
        format!("{} {}", ours(1), "z".repeat(900)),
        format!("{} \u{202e}reversed\u{200b}", ours(2)),
        "-1.000s ERROR sidevoice_connector: a connector line in a core record",
        "-1.000s ERROR panic: the panic's own line",
        "-1.000s  WARN hyper_util::client: GET https://x/?token=canary",
        "-1.000s DEBUG sidevoice_core::x: a debug line",
        "-1.000s  WARN sidevoice_corevoice: a lookalike target",
        "2026-11-15T10:00:00.123456Z ERROR sidevoice_core::x: a dated line",
        "raw stderr of a service ?token=canary",
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.log_tail.len(), 2, "{:?}", read.log_tail);
    assert!(
        read.log_tail[0].chars().count() <= 600 && read.log_tail[0].ends_with("zzzz"),
        "cut to the limit"
    );
    assert!(read.log_tail[1].ends_with("reversed"));
    assert!(!read.log_tail[1].contains('\u{202e}'));
}

#[test]
fn a_warning_inside_a_span_is_kept_and_a_dependency_inside_a_span_is_dropped() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([
        "-1.000s  WARN binding{id=\"a: b}\" n=3}: sidevoice_core::room: kept one",
        "-1.000s  WARN outer{a=1}: inner: sidevoice_core::room: kept two",
        "-1.000s  WARN binding{id=1}: hyper_util::client: dropped one",
        "-1.000s  WARN binding{id=1}: sidevoice_corevoice: dropped two",
        "-1.000s  WARN binding{id=1} sidevoice_core::room: dropped three",
    ]);
    let read = CrashRecord::from_untrusted(&raw, &[]).unwrap();
    assert_eq!(read.log_tail.len(), 2, "{:?}", read.log_tail);
    assert!(read.log_tail[1].contains("kept two"));
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

#[test]
fn format_characters_are_stripped_before_the_home_is_masked() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["log_tail"] = json!([format!("{} at /data/al\u{200b}ice/x", ours(1))]);
    let read = CrashRecord::from_untrusted(&raw, &["/data/alice".to_owned()]).unwrap();
    assert!(read.log_tail[0].ends_with("at ~/x"), "{:?}", read.log_tail);
}

#[test]
fn a_date_past_year_9999_is_not_taken() {
    let mut raw = serde_json::to_value(record(1)).unwrap();
    raw["first_at"] = json!("9999-12-31T23:59:59-23:59");
    raw["last_at"] = json!("9999-12-31T23:59:59-23:59");
    assert!(CrashRecord::from_untrusted(&raw, &[]).is_none());
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

// The store.

#[test]
fn the_same_signature_keeps_one_record_with_a_count_and_the_times() {
    let f = fixture();
    let first = record(5);
    f.store.record(first.clone()).unwrap();
    let mut again = record(5);
    again.last_at = at(-1);
    f.store.record(again).unwrap();
    f.store.record(record(5)).unwrap();
    let listed = f.store.pending().unwrap();
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
    let listed = f.store.pending().unwrap();
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
    let listed = f.store.pending().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].location.line, 2);
    assert_eq!(names(&f.data.join("crashes")).len(), 1);
}

#[test]
fn the_largest_possible_records_fit_the_caps() {
    let f = fixture();
    let worst = |line: u32| {
        let mut raw = serde_json::to_value(record(line)).unwrap();
        raw["log_tail"] = json!(vec![format!("{} {}", ours(0), "x".repeat(600)); TAIL_LINES]);
        raw["backtrace"] = json!(vec![
            json!({"function": "y".repeat(240), "file": "f".repeat(100), "line": u32::MAX, "addr": u64::MAX});
            MAX_FRAMES
        ]);
        raw["images"] = json!(vec![
            json!({"name": "n".repeat(100), "addr": u64::MAX, "size": u64::MAX, "vmaddr": u64::MAX,
                   "id": "7b2f0b00-1c3e-4d5a-8f6b-0123456789ab-ffffffff", "code_id": "a".repeat(128)});
            MAX_IMAGES
        ]);
        raw["location"]["line"] = json!(line);
        raw["last_at"] = json!(at(i64::from(line) % 3));
        CrashRecord::from_untrusted(&raw, &[]).unwrap()
    };
    for line in 0..MAX_RECORDS as u32 {
        f.store.record(worst(line)).unwrap();
    }
    let bytes: u64 = fs::read_dir(f.data.join("crashes"))
        .unwrap()
        .flatten()
        .map(|entry| entry.metadata().unwrap().len())
        .sum();
    assert!(bytes <= MAX_BYTES, "{bytes}");
    assert_eq!(f.store.pending().unwrap().len(), MAX_RECORDS);
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
    let listed = f.store.pending().unwrap();
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
fn removing_deletes_the_record_and_only_by_its_id() {
    let f = fixture();
    let kept = record(1);
    f.store.record(kept.clone()).unwrap();
    let outside = f.data.join("crashes").join("..").join("crash-x.json");
    fs::write(&outside, b"mine").unwrap();
    f.store.remove("../crash-x").unwrap();
    f.store.remove(&kept.id.to_uppercase()).unwrap();
    assert!(outside.exists(), "nothing but a UUID names a file");
    assert_eq!(f.store.pending().unwrap().len(), 1);
    f.store.remove(&kept.id).unwrap();
    f.store.remove(&kept.id).unwrap();
    assert!(f.store.pending().unwrap().is_empty());
}

// The switch and the machine id.

#[test]
fn the_switch_is_on_by_default_and_off_erases_the_id_and_the_records() {
    let f = fixture();
    assert!(enabled(&f.data));
    assert!(setting(&f.data).unwrap());
    f.store.record(record(1)).unwrap();
    let id = settings::machine_id(&f.data).unwrap();
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(settings::machine_id(&f.data).unwrap(), id, "kept");

    set_enabled(&f.data, false).unwrap();
    assert!(!enabled(&f.data));
    assert_eq!(
        names(&f.data),
        ["crash-reports.json"],
        "only the switch is left"
    );
    assert!(f.store.pending().unwrap().is_empty());

    set_enabled(&f.data, true).unwrap();
    assert!(enabled(&f.data));
    assert_ne!(settings::machine_id(&f.data).unwrap(), id, "a new id");
}

#[test]
fn a_switch_that_cannot_be_read_is_off_and_is_left_as_it_is() {
    let f = fixture();
    Files::write_private(&f.data.join("crash-reports.json"), b"not json").unwrap();
    assert!(setting(&f.data).is_err());
    assert!(!enabled(&f.data));
    Files::write_private(&f.data.join("crash-reports.json"), br#"{"send": "yes"}"#).unwrap();
    assert!(!enabled(&f.data));
    assert_eq!(
        fs::read(f.data.join("crash-reports.json")).unwrap(),
        br#"{"send": "yes"}"#
    );
    set_enabled(&f.data, true).unwrap();
    assert!(enabled(&f.data), "the next choice replaces it");
}

#[test]
fn the_machine_id_is_replaced_after_a_year() {
    let f = fixture();
    let id = settings::machine_id(&f.data).unwrap();
    settings::identity_made(&f.data, &at(364));
    assert_eq!(settings::machine_id(&f.data).unwrap(), "0".repeat(32));
    settings::identity_made(&f.data, &at(366));
    let next = settings::machine_id(&f.data).unwrap();
    assert_ne!(next, "0".repeat(32));
    assert_ne!(next, id);
    assert_eq!(settings::machine_id(&f.data).unwrap(), next, "and kept");
}

// The capture.

#[test]
fn a_capture_makes_a_record_with_addresses_and_none_while_the_switch_is_off() {
    let f = fixture();
    let context = context(&f);
    let capture = |literal| {
        write(
            &context,
            literal,
            0,
            "rust/server/x.rs",
            3,
            backtrace::Backtrace::new_unresolved(),
        );
    };
    // The store's lock is the process's: a capture that finds it held by another test drops its record, as it should.
    let mut records = Vec::new();
    for _ in 0..200 {
        capture(Some("boom"));
        records = f.store.pending().unwrap();
        if !records.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].exception.message, "boom");
    assert!(!records[0].backtrace.is_empty());
    assert!(records[0].backtrace.iter().all(|frame| frame.addr != 0));

    set_enabled(&f.data, false).unwrap();
    capture(Some("again"));
    assert!(f.store.pending().unwrap().is_empty(), "none while off");
}

#[test]
fn frames_start_at_the_code_that_panicked_and_images_are_the_ones_that_hold_a_frame() {
    let f = |function: &str, addr| frame(function, addr);
    let trimmed = trace::trim(vec![
        f("backtrace::capture::Backtrace::new_unresolved", 1),
        f(
            "sidevoice_core::runtime::logging::panic_hook::{{closure}}",
            2,
        ),
        f("std::panicking::rust_panic_with_hook", 3),
        f("std::sys::backtrace::__rust_end_short_backtrace", 4),
        f("rust_begin_unwind", 5),
        f("core::panicking::panic_fmt", 6),
        f("sidevoice_core::x::boom", 7),
        f("std::panicking::try::do_call", 8),
    ]);
    let functions: Vec<_> = trimmed
        .iter()
        .map(|frame| frame.function.clone().unwrap())
        .collect();
    assert_eq!(
        functions,
        ["sidevoice_core::x::boom", "std::panicking::try::do_call"]
    );
    assert_eq!(
        trace::trim(vec![f("a", 1), f("b", 2)]).len(),
        2,
        "no machinery, no cut"
    );

    let images = [image(0x1000, 0x1000), image(0x9000, 0x100)];
    let held = trace::holding(&images, &[frame("a", 0x1ff0), frame("b", 0x20_0000)]);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].addr, 0x1000);
    assert!(
        trace::holding(&images, &[frame("a", 0x2000)]).is_empty(),
        "past the end"
    );

    let mut here = backtrace::Backtrace::new();
    here.resolve();
    assert!(trace::frames(&here).iter().all(|frame| frame.addr != 0));
    // The program's own module is listed with a bare name, when it has a debug id.
    assert!(trace::loaded()
        .iter()
        .all(|image| !image.name.contains(['/', '\\'])));
}

// The event.

fn everything() -> CrashRecord {
    let mut raw = panic(None, 7);
    raw.formatted_len = 42;
    raw.file = "/home/canary-user-7f3a/.cargo/registry/src/x/a.rs";
    raw.frames = vec![
        Frame {
            function: Some("a::b".to_owned()),
            file: Some("/home/canary-user-7f3a/src/a.rs".to_owned()),
            line: Some(3),
            addr: 0x1010,
        },
        Frame {
            function: None,
            file: None,
            line: None,
            addr: 0x1020,
        },
    ];
    raw.images = vec![Image {
        name: "/home/canary-user-7f3a/.local/share/sidevoice/bin/sidevoice-core".to_owned(),
        ..image(0x1000, 0x1000)
    }];
    raw.tail = vec![format!(
        "-0.250s ERROR sidevoice_core::x: failed device=d1 path=/home/canary-user-7f3a/x {}",
        "ok"
    )];
    CrashRecord::from_panic(raw, &["/home/canary-user-7f3a".to_owned()])
}

#[test]
fn the_event_carries_the_closed_field_list_and_nothing_else() {
    let record = everything();
    let machine = "0123456789abcdef0123456789abcdef";
    let value = serde_json::to_value(event(&record, machine)).unwrap();
    assert_eq!(
        keys(&value),
        [
            "debug_meta",
            "event_id",
            "exception",
            "extra",
            "level",
            "platform",
            "release",
            "tags",
            "timestamp"
        ],
        "{value}"
    );
    assert!(
        value.get("server_name").is_none(),
        "never the host name: {value}"
    );
    assert_eq!(value["event_id"], record.id.replace('-', "").as_str());
    assert_eq!(value["level"], "fatal");
    assert_eq!(value["release"], "sidevoice-core@1.2.3");
    assert_eq!(keys(&value["tags"]), {
        let mut tags = TAGS.to_vec();
        tags.sort_unstable();
        tags
    });
    assert_eq!(value["tags"]["service.name"], "sidevoice-core");
    assert_eq!(value["tags"]["installation.id"], machine);
    let mut extras = EXTRAS.to_vec();
    extras.sort_unstable();
    assert_eq!(keys(&value["extra"]), extras);
    assert_eq!(value["extra"]["len"], 42);
    for local in ["count", "first_at"] {
        assert!(
            value["extra"].get(local).is_none(),
            "{local} stays on the machine"
        );
    }
    assert_eq!(value["extra"]["location"], "unknown:7");
    let exception = &value["exception"]["values"][0];
    assert_eq!(keys(exception), ["stacktrace", "type", "value"]);
    assert_eq!(exception["type"], "panic");
    assert_eq!(exception["value"], FORMATTED_MARKER);
    let frames = exception["stacktrace"]["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    for frame in frames {
        assert!(
            keys(frame)
                .iter()
                .all(|key| ["filename", "function", "instruction_addr", "lineno"].contains(key)),
            "{frame}"
        );
    }
    // Outermost first, as Sentry reads it.
    assert_eq!(frames[1]["function"], "a::b");
    assert_eq!(frames[1]["filename"], "a.rs");
    assert_eq!(frames[1]["instruction_addr"], "0x1010");
    let images = value["debug_meta"]["images"].as_array().unwrap();
    assert_eq!(images.len(), 1);
    assert!(
        keys(&images[0]).iter().all(|key| [
            "arch",
            "code_id",
            "id",
            "image_addr",
            "image_size",
            "image_vmaddr",
            "name",
            "type"
        ]
        .contains(key)),
        "{}",
        images[0]
    );
    assert_eq!(images[0]["name"], "sidevoice-core");
    assert_eq!(images[0]["id"], ID);
    // Without a formatted message there is no length.
    let literal = serde_json::to_value(event(&record_with_literal(), machine)).unwrap();
    assert!(literal["extra"].get("len").is_none());
}

fn record_with_literal() -> CrashRecord {
    record(1)
}

#[test]
fn no_home_or_install_path_reaches_the_event() {
    let json = serde_json::to_string(&event(&everything(), "00")).unwrap();
    for canary in ["canary-user", "/home/", ".cargo", ".local", "registry"] {
        assert!(!json.contains(canary), "{canary} in {json}");
    }
    // Nor the name of the machine (a short one could be a word of the event's own).
    use crate::platform::user::{User, UserId};
    let host = UserId::host_name();
    if host.len() >= 6 {
        assert!(!json.contains(&host), "the host name is in {json}");
    }
}

// The sender.

fn dsn(server: &MockServer) -> sentry::types::Dsn {
    format!("http://public@{}/42", server.address())
        .parse()
        .unwrap()
}

async fn envelope_server(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/42/envelope/"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_record_is_sent_under_its_own_id_and_deleted_once_sentry_took_it() {
    let f = fixture();
    let server = envelope_server(200).await;
    let kept = record(1);
    f.store.record(kept.clone()).unwrap();
    f.store.record(record(2)).unwrap();
    assert_eq!(drain(&context(&f), &dsn(&server)).await, 2);
    assert!(f.store.pending().unwrap().is_empty());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let body = String::from_utf8_lossy(&requests[0].body).into_owned();
    let bodies: String = requests
        .iter()
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect();
    assert!(bodies.contains(&kept.id.replace('-', "")), "{bodies}");
    assert!(body.contains("installation.id"));
    assert!(requests[0].headers.contains_key("x-sentry-auth"));
    assert_eq!(
        requests[0].headers["content-type"],
        "application/x-sentry-envelope"
    );
}

#[tokio::test]
async fn a_failed_send_keeps_the_record_and_ends_the_pass() {
    let f = fixture();
    let server = envelope_server(500).await;
    f.store.record(record(1)).unwrap();
    f.store.record(record(2)).unwrap();
    assert_eq!(drain(&context(&f), &dsn(&server)).await, 0);
    assert_eq!(f.store.pending().unwrap().len(), 2);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "one failure and no more tries this start"
    );
    // A server that is not there is the same: nothing panics, nothing is lost.
    let gone = {
        let server = MockServer::start().await;
        let dsn = dsn(&server);
        drop(server);
        dsn
    };
    assert_eq!(drain(&context(&f), &gone).await, 0);
    assert_eq!(f.store.pending().unwrap().len(), 2);
}

#[tokio::test]
async fn nothing_leaves_a_machine_set_to_off() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    f.store.record(record(1)).unwrap();
    Files::write_json(&f.data.join("crash-reports.json"), &json!({"send": false})).unwrap();
    assert_eq!(drain(&context, &dsn(&server)).await, 0);
    let client = reqwest::Client::new();
    assert!(matches!(
        deliver(&context, &client, &dsn(&server), event(&record(2), "00")).await,
        Err(Undelivered::Off)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
    // The pass at start erases what a machine set to off still has, with or without a DSN.
    pass(&context, None);
    assert!(f.store.pending().unwrap().is_empty());
    assert!(!f.data.join("crash-machine-id.json").exists());
}

#[test]
fn a_pass_without_a_dsn_sends_nothing_and_keeps_the_records() {
    let f = fixture();
    f.store.record(record(1)).unwrap();
    pass(&context(&f), None);
    assert_eq!(f.store.pending().unwrap().len(), 1);
    assert!(
        !f.data.join("crash-machine-id.json").exists(),
        "no id is made when nothing is sent"
    );
}

#[tokio::test]
async fn a_store_that_cannot_be_read_is_one_log_line_and_no_panic() {
    let f = fixture();
    let server = envelope_server(200).await;
    // A file where the records' directory should be.
    fs::write(f.data.join("crashes"), b"not a directory").unwrap();
    assert_eq!(drain(&context(&f), &dsn(&server)).await, 0);
    assert!(server.received_requests().await.unwrap().is_empty());
    pass(&context(&f), None);
}

// The races of turning sending off.

#[test]
fn nothing_is_kept_or_made_after_the_switch_was_written_off() {
    let f = fixture();
    // The switch alone, as a person's `off` writes it before it erases: what comes after finds it off, under the store's lock.
    settings::write(&f.data, false).unwrap();
    f.store.record(record(1)).unwrap();
    f.store.record_without_waiting(record(2));
    assert!(!f.data.join("crashes").exists(), "no record after off");
    // The id is made and then looked at again: a sender that read the switch before `off` is not left holding an id.
    assert!(settings::machine_id(&f.data).is_err());
    assert!(!f.data.join("crash-machine-id.json").exists());
    // On again: both work.
    settings::write(&f.data, true).unwrap();
    f.store.record(record(3)).unwrap();
    assert_eq!(f.store.pending().unwrap().len(), 1);
    assert!(settings::machine_id(&f.data).is_ok());
}

#[test]
fn off_erases_a_store_that_is_being_written_into() {
    let f = fixture();
    for line in 0..MAX_RECORDS as u32 {
        f.store.record(record(line)).unwrap();
    }
    let writer = {
        let dir = f.data.join("crashes");
        std::thread::spawn(move || {
            for n in 0..2000 {
                let _ = fs::write(dir.join(format!("noise-{n}")), b"x");
            }
        })
    };
    // Whatever the writer is doing to the directory, turning off is not an error and the switch is off.
    for _ in 0..20 {
        set_enabled(&f.data, false).unwrap();
        assert!(!enabled(&f.data));
        set_enabled(&f.data, true).unwrap();
        f.store.record(record(1)).unwrap();
    }
    writer.join().unwrap();
    set_enabled(&f.data, false).unwrap();
    assert!(f.store.pending().unwrap().is_empty());
}
