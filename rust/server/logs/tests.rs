//! The log's pages (the reader, over real private files, rotated as the writer rotates them) and `GET /api/logs`
//! (what each listener lets through, holding on for a line, who may hold).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

use super::reader::{read, Page, Refused};
use super::Logs;
use crate::control::room::Room;
use crate::messages::{render, LocalizedMessage};
use crate::platform::fs::{Create, Files, Fs};
use crate::server::tests::support::{app_state, private_dir};
use crate::server::{router, AppState};

fn files(log: &Path) -> Vec<PathBuf> {
    ["log.2", "log.1", "log"]
        .map(|extension| log.with_extension(extension))
        .to_vec()
}

fn append(path: &Path, text: &str) {
    let mut file = Files::open_private(path, Create::Append).unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// What the writer does when the log is full: the older generation is dropped, the others move up.
fn rotate(log: &Path) {
    let [second, first] = ["log.2", "log.1"].map(|extension| log.with_extension(extension));
    let _ = fs::remove_file(&second);
    if first.exists() {
        fs::rename(&first, &second).unwrap();
    }
    fs::rename(log, &first).unwrap();
}

fn texts(page: &Page) -> Vec<&str> {
    page.lines.iter().map(|line| line.text.as_str()).collect()
}

/// A log in a private directory that lives as long as the guard.
fn log() -> (TempDir, PathBuf, Vec<PathBuf>) {
    let (temp, dir) = private_dir();
    let log = dir.path().join("core.log");
    let files = files(&log);
    (temp, log, files)
}

fn numbered(from: usize, to: usize) -> String {
    (from..to).map(|n| format!("line {n}\n")).collect()
}

// --- The reader ---------------------------------------------------------------------------------

#[test]
fn an_empty_log_answers_with_the_start_cursor_and_no_line() {
    let (_temp, log, files) = log();
    let page = read(&files, None, 10).unwrap();
    assert!(page.lines.is_empty() && !page.more);
    assert_eq!(page.cursor, "start");
    append(&log, "");
    let page = read(&files, Some("start"), 10).unwrap();
    assert!(page.lines.is_empty());
    assert_eq!(page.cursor, "start");
}

#[test]
fn the_tail_spans_the_rotated_generations_and_ends_at_the_end() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    rotate(&log);
    append(&log, &numbered(3, 5));
    let page = read(&files, None, 4).unwrap();
    assert_eq!(texts(&page), ["line 1", "line 2", "line 3", "line 4"]);
    assert!(!page.more);
    assert_eq!(read(&files, None, 1000).unwrap().lines.len(), 5);
    assert!(read(&files, Some(&page.cursor), 10)
        .unwrap()
        .lines
        .is_empty());
    append(&log, "line 5\n");
    let next = read(&files, Some(&page.cursor), 10).unwrap();
    assert_eq!(texts(&next), ["line 5"]);
}

#[test]
fn the_tail_of_a_big_log_is_whole_lines_within_the_page_size() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 20_000));
    let page = read(&files, None, 1000).unwrap();
    assert_eq!(page.lines.len(), 1000);
    assert_eq!(page.lines[0].text, "line 19000");
    assert_eq!(page.lines[999].text, "line 19999");
    let long = "x".repeat(1000);
    let (_temp, log, files) = self::log();
    append(&log, &format!("{long}\n").repeat(2000));
    let page = read(&files, None, 1000).unwrap();
    assert!(
        (200..300).contains(&page.lines.len()),
        "{}",
        page.lines.len()
    );
    assert!(page.lines.iter().all(|line| line.text == long));
}

#[test]
fn a_page_is_at_most_limit_lines_and_the_cursor_pages_through_without_loss_or_repeat() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 10));
    let mut seen = Vec::new();
    let mut after = Some("start".to_owned());
    let mut pages = 0;
    loop {
        let page = read(&files, after.as_deref(), 4).unwrap();
        assert!(page.lines.len() <= 4);
        seen.extend(page.lines.iter().map(|line| line.text.clone()));
        pages += 1;
        after = Some(page.cursor);
        if !page.more {
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(
        seen,
        (0..10).map(|n| format!("line {n}")).collect::<Vec<_>>()
    );
}

#[test]
fn a_line_has_one_id_however_it_is_read_again() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 6));
    let first = read(&files, Some("start"), 1000).unwrap();
    let tail = read(&files, None, 1000).unwrap();
    let ids: Vec<_> = first.lines.iter().map(|line| &line.id).collect();
    assert_eq!(
        ids,
        tail.lines.iter().map(|line| &line.id).collect::<Vec<_>>()
    );
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 6);
    // It stays when the log rotates under it, and a cursor and an id are the same short, URL-safe thing.
    rotate(&log);
    append(&log, "line 6\n");
    let again = read(&files, Some("start"), 1000).unwrap();
    assert_eq!(
        again.lines[..6]
            .iter()
            .map(|line| &line.id)
            .collect::<Vec<_>>(),
        ids
    );
    for text in again
        .lines
        .iter()
        .map(|line| &line.id)
        .chain([&again.cursor])
    {
        assert!(text.len() <= 128);
        assert!(text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'));
    }
}

#[test]
fn a_cursor_goes_on_through_the_generations_after_a_rotation_without_loss_or_repeat() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    let cursor = read(&files, None, 100).unwrap().cursor;
    append(&log, &numbered(3, 5));
    rotate(&log);
    append(&log, &numbered(5, 8));
    let page = read(&files, Some(&cursor), 100).unwrap();
    assert_eq!(
        texts(&page),
        ["line 3", "line 4", "line 5", "line 6", "line 7"]
    );
    // Once more, with the cursor still in the first generation: now the second older one.
    rotate(&log);
    append(&log, &numbered(8, 9));
    let page = read(&files, Some(&cursor), 100).unwrap();
    assert_eq!(texts(&page).len(), 6);
    assert_eq!(page.lines[0].text, "line 3");
    assert_eq!(page.lines[5].text, "line 8");
    // A page that ends exactly where a generation ends goes on in the next.
    let ends = read(&files, Some(&cursor), 2).unwrap();
    assert_eq!(texts(&ends), ["line 3", "line 4"]);
    assert!(ends.more);
    let rest = read(&files, Some(&ends.cursor), 100).unwrap();
    assert_eq!(texts(&rest), ["line 5", "line 6", "line 7", "line 8"]);
    assert!(!rest.more);
}

#[test]
fn a_file_seen_under_two_names_is_one_generation_and_a_log_between_rotations_is_continuous() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    // A rotation between two opens shows one file twice.
    let twice = [log.clone(), log.clone()];
    assert_eq!(texts(&read(&twice, Some("start"), 10).unwrap()).len(), 3);
    // Renamed, and the new log not yet there.
    rotate(&log);
    let page = read(&files, None, 10).unwrap();
    assert_eq!(texts(&page), ["line 0", "line 1", "line 2"]);
    assert!(read(&files, Some(&page.cursor), 10)
        .unwrap()
        .lines
        .is_empty());
    append(&log, "line 3\n");
    let next = read(&files, Some(&page.cursor), 10).unwrap();
    assert_eq!(texts(&next), ["line 3"]);
}

#[test]
fn a_cursor_whose_generation_was_deleted_is_expired() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    let cursor = read(&files, None, 100).unwrap().cursor;
    for round in 0..3 {
        rotate(&log);
        append(&log, &numbered(10 * (round + 1), 10 * (round + 1) + 2));
        let held = read(&files, Some(&cursor), 100);
        assert_eq!(held.is_ok(), round < 2, "after {} rotations", round + 1);
    }
    assert_eq!(
        read(&files, Some(&cursor), 100).err(),
        Some(Refused::Expired)
    );
    // And one that was never there.
    assert_eq!(
        read(&files, Some("0123456789abcdef.0"), 100).err(),
        Some(Refused::Expired)
    );
}

#[test]
fn a_cursor_past_the_end_of_its_file_is_expired() {
    let (_temp, log, files) = log();
    append(&log, &numbered(0, 3));
    let (id, offset) = read(&files, None, 100)
        .unwrap()
        .cursor
        .split_once('.')
        .map(|(i, o)| (i.to_owned(), o.parse::<u64>().unwrap()))
        .unwrap();
    for past in [offset + 1, offset + 100] {
        assert_eq!(
            read(&files, Some(&format!("{id}.{past}")), 10).err(),
            Some(Refused::Expired)
        );
    }
    // Where it ends, and where it starts, are fine.
    assert!(read(&files, Some(&format!("{id}.{offset}")), 10)
        .unwrap()
        .lines
        .is_empty());
    assert_eq!(
        read(&files, Some(&format!("{id}.0")), 10)
            .unwrap()
            .lines
            .len(),
        3
    );
}

#[test]
fn a_malformed_cursor_is_invalid() {
    let (_temp, log, files) = log();
    append(&log, "alpha\nbeta\n");
    let valid = read(&files, None, 10).unwrap().cursor;
    let id = valid.split_once('.').unwrap().0;
    let too_long = format!("{id}.{}", "1".repeat(130));
    for bad in [
        "",
        "x",
        "start.",
        "start0",
        ".",
        "0123456789abcdef",
        "0123456789abcdef.",
        "0123456789abcde.1",
        "0123456789abcdefg.1",
        "zzzzzzzzzzzzzzzz.1",
        "0123456789abcdef.-1",
        "0123456789abcdef.+1",
        "0123456789abcdef.1.2",
        "0123456789abcdef.1x",
        "../../etc/passwd",
        &too_long,
        // Right generation, but in the middle of a line.
        &format!("{id}.3"),
    ] {
        assert_eq!(
            read(&files, Some(bad), 10).err(),
            Some(Refused::Invalid),
            "{bad:?}"
        );
    }
}

#[test]
fn a_torn_last_line_is_never_delivered_and_is_whole_when_it_is_finished() {
    let (_temp, log, files) = log();
    append(&log, "alpha\nbeta\ngam");
    let page = read(&files, None, 10).unwrap();
    assert_eq!(texts(&page), ["alpha", "beta"]);
    assert_eq!(
        texts(&read(&files, Some("start"), 10).unwrap()),
        ["alpha", "beta"]
    );
    assert!(read(&files, Some(&page.cursor), 10)
        .unwrap()
        .lines
        .is_empty());
    append(&log, "ma\ndelta\n");
    let next = read(&files, Some(&page.cursor), 10).unwrap();
    assert_eq!(texts(&next), ["gamma", "delta"]);
    // A generation that is only a torn line has nothing to give yet.
    let (_temp, log, files) = self::log();
    append(&log, "never ended");
    let page = read(&files, None, 10).unwrap();
    assert!(page.lines.is_empty());
    assert_eq!(page.cursor, "start");
}

#[test]
fn a_page_stops_at_its_size_but_always_holds_a_whole_line() {
    let (_temp, log, files) = log();
    let big = "b".repeat(100_000);
    append(&log, &format!("{big}\n").repeat(5));
    let mut sizes = Vec::new();
    let mut after = "start".to_owned();
    loop {
        let page = read(&files, Some(&after), 1000).unwrap();
        sizes.push(page.lines.len());
        after = page.cursor;
        if !page.more {
            break;
        }
    }
    assert_eq!(sizes, [2, 2, 1]);
    let (_temp, log, files) = self::log();
    let huge = "h".repeat(300_000);
    append(&log, &format!("{huge}\nsmall\n"));
    let page = read(&files, Some("start"), 1000).unwrap();
    assert_eq!(texts(&page), [huge.as_str()]);
    assert!(page.more);
    let tail = read(&files, None, 1000).unwrap();
    assert_eq!(texts(&tail), ["small"]);
}

#[test]
fn a_line_is_its_text_without_the_line_ending() {
    let (_temp, log, files) = log();
    append(&log, "one\r\ntwo\n\nlast words \n");
    let page = read(&files, None, 10).unwrap();
    assert_eq!(texts(&page), ["one", "two", "", "last words "]);
}

// --- GET /api/logs ------------------------------------------------------------------------------

struct Node {
    _temp: TempDir,
    log: PathBuf,
    state: Arc<AppState>,
}

impl Node {
    fn new() -> Self {
        Self::timed(Duration::from_secs(25), Duration::from_millis(250))
    }

    fn timed(max_wait: Duration, poll: Duration) -> Self {
        Self::shaped(|logs| logs.timed(max_wait, poll))
    }

    fn shaped(shape: impl FnOnce(Logs) -> Logs) -> Self {
        let (temp, dir) = private_dir();
        let log = dir.path().join("core.log");
        let room = Arc::new(Room::load(dir.clone()).unwrap());
        let mut state = app_state(&dir, room, "fixture-host");
        Arc::get_mut(&mut state).unwrap().logs = shape(Logs::new(files(&log)));
        Self {
            _temp: temp,
            log,
            state,
        }
    }

    fn tcp(&self) -> Router {
        router(self.state.clone(), false)
    }

    fn socket(&self) -> Router {
        router(self.state.clone(), true)
    }

    /// A device paired with a one-time code over TCP: its id and token.
    async fn device(&self) -> (String, String) {
        let code = self.state.issue_code();
        let secret = code["payload"]["secret"].as_str().unwrap();
        let (status, _, body) = send(
            &self.tcp(),
            "POST",
            "/api/device/pair",
            None,
            &[],
            Some(json!({"secret": secret})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (
            body["device_id"].as_str().unwrap().to_owned(),
            body["token"].as_str().unwrap().to_owned(),
        )
    }

    fn revoke(&self, device: &str) {
        let revoked = self.state.registry.lock().unwrap().revoke(device).unwrap();
        assert!(revoked);
    }
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:10829");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = to_bytes(response.into_body(), 1 << 22).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

async fn get(app: &Router, path: &str, token: &str) -> (StatusCode, HeaderMap, Value) {
    send(app, "GET", path, Some(token), &[], None).await
}

fn refusal(key: &str) -> Value {
    json!(render(&LocalizedMessage::new(key), "en"))
}

fn texts_of(body: &Value) -> Vec<&str> {
    body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line["text"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn the_log_needs_a_paired_device_on_both_listeners_and_is_not_a_local_path() {
    let node = Node::new();
    append(&node.log, "hello\n");
    let (_, token) = node.device().await;
    for app in [node.tcp(), node.socket()] {
        for headers in [&[][..], &[("authorization", "Bearer not-a-token")][..]] {
            let (status, response, body) =
                send(&app, "GET", "/api/logs", None, headers, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(body, json!({"detail": refusal("device.unpaired")}));
            assert_eq!(response.get("www-authenticate").unwrap(), "Bearer");
        }
        let (status, _, body) = get(&app, "/api/logs", &token).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(texts_of(&body), ["hello"]);
        assert_eq!(body["more"], false);
    }
    // Only a read: no other method gets in, and the path is not under a local prefix.
    let (status, ..) = send(
        &node.tcp(),
        "POST",
        "/api/logs",
        Some(&token),
        &[],
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(!crate::server::trust::local_only("/api/logs"));
    assert!(!crate::server::guard::open_route(
        &axum::http::Method::GET,
        "/api/logs",
        true
    ));
}

#[tokio::test]
async fn a_foreign_origin_is_refused_even_with_a_token() {
    let node = Node::new();
    let (_, token) = node.device().await;
    let (status, _, body) = send(
        &node.socket(),
        "GET",
        "/api/logs",
        Some(&token),
        &[("origin", "https://elsewhere.example")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body, json!({"detail": refusal("request.origin_invalid")}));
}

#[tokio::test]
async fn a_request_that_is_not_valid_is_refused_and_what_is_unknown_is_ignored() {
    let node = Node::new();
    append(&node.log, &numbered(0, 3));
    let (_, token) = node.device().await;
    for query in [
        "limit=0",
        "limit=1001",
        "limit=-1",
        "limit=many",
        "limit=",
        "wait=soon",
        "wait=-1",
        "after=",
        "after=garbage",
        "after=0123456789abcdef.-1",
    ] {
        let (status, _, body) = get(&node.tcp(), &format!("/api/logs?{query}"), &token).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body, json!({"detail": refusal("logs.request_invalid")}));
    }
    let (status, _, body) = get(&node.tcp(), "/api/logs?limit=1&other=1&limit=5", &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(texts_of(&body), ["line 2"]);
    let (status, ..) = get(&node.tcp(), "/api/logs?limit=1000&wait=0", &token).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_log_is_read_as_a_tail_then_by_cursor_and_an_expired_cursor_is_gone() {
    let node = Node::new();
    append(&node.log, &numbered(0, 5));
    let (_, token) = node.device().await;
    let (_, _, tail) = get(&node.tcp(), "/api/logs?limit=2", &token).await;
    assert_eq!(texts_of(&tail), ["line 3", "line 4"]);
    let cursor = tail["cursor"].as_str().unwrap().to_owned();
    assert!(tail["lines"][0]["id"].as_str().unwrap().len() <= 128);
    append(&node.log, &numbered(5, 9));
    let (_, _, page) = get(
        &node.tcp(),
        &format!("/api/logs?limit=3&after={cursor}"),
        &token,
    )
    .await;
    assert_eq!(texts_of(&page), ["line 5", "line 6", "line 7"]);
    assert_eq!(page["more"], true);
    let (_, _, rest) = get(
        &node.tcp(),
        &format!("/api/logs?after={}", page["cursor"].as_str().unwrap()),
        &token,
    )
    .await;
    assert_eq!(texts_of(&rest), ["line 8"]);
    assert_eq!(rest["more"], false);
    for _ in 0..3 {
        rotate(&node.log);
        append(&node.log, "later\n");
    }
    let (status, _, body) = get(&node.tcp(), &format!("/api/logs?after={cursor}"), &token).await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(body, json!({"detail": refusal("logs.cursor_expired")}));
}

#[tokio::test]
async fn a_generation_that_cannot_be_read_is_unavailable_not_empty() {
    let node = Node::new();
    // A generation that has another name is one somebody else can write to: refused, not read.
    append(&node.log, "hello\n");
    let (_, token) = node.device().await;
    let (status, _, body) = get(&node.tcp(), "/api/logs", &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    fs::hard_link(&node.log, node.log.with_extension("log.1")).unwrap();
    let (status, _, body) = get(&node.tcp(), "/api/logs", &token).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body, json!({"detail": refusal("logs.unreadable")}));
}

#[tokio::test]
async fn a_read_that_waits_returns_at_the_clamp_with_the_cursor_unchanged() {
    let node = Node::timed(Duration::from_millis(400), Duration::from_millis(20));
    append(&node.log, &numbered(0, 2));
    let (_, token) = node.device().await;
    let (_, _, tail) = get(&node.tcp(), "/api/logs", &token).await;
    let cursor = tail["cursor"].as_str().unwrap();
    let started = Instant::now();
    let (status, _, body) = get(
        &node.tcp(),
        &format!("/api/logs?after={cursor}&wait=3600"),
        &token,
    )
    .await;
    let waited = started.elapsed();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["lines"].as_array().unwrap().is_empty());
    assert_eq!(body["cursor"], cursor);
    assert_eq!(body["more"], false);
    assert!(
        waited >= Duration::from_millis(350) && waited < Duration::from_secs(3),
        "{waited:?}"
    );
    // Asking for less, it returns sooner.
    let started = Instant::now();
    get(
        &node.tcp(),
        &format!("/api/logs?after={cursor}&wait=0"),
        &token,
    )
    .await;
    assert!(started.elapsed() < Duration::from_millis(300));
}

#[tokio::test]
async fn a_new_line_wakes_a_read_that_waits_and_so_does_the_first_line_of_an_empty_log() {
    let node = Node::timed(Duration::from_secs(10), Duration::from_millis(20));
    let (_, token) = node.device().await;
    let writer = |log: PathBuf, text: &'static str| {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            append(&log, text);
        })
    };
    // Empty log: the tail itself waits.
    let wake = writer(node.log.clone(), "first\n");
    let started = Instant::now();
    let (_, _, body) = get(&node.tcp(), "/api/logs?wait=10", &token).await;
    wake.await.unwrap();
    assert_eq!(texts_of(&body), ["first"]);
    assert!(started.elapsed() < Duration::from_secs(5));
    // With a cursor.
    let cursor = body["cursor"].as_str().unwrap().to_owned();
    let wake = writer(node.log.clone(), "second\n");
    let started = Instant::now();
    let (_, _, body) = get(
        &node.tcp(),
        &format!("/api/logs?after={cursor}&wait=10"),
        &token,
    )
    .await;
    wake.await.unwrap();
    assert_eq!(texts_of(&body), ["second"]);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_line_that_is_not_finished_does_not_wake_a_read_that_waits() {
    let node = Node::timed(Duration::from_millis(400), Duration::from_millis(20));
    append(&node.log, "whole\nhalf a li");
    let (_, token) = node.device().await;
    let (_, _, tail) = get(&node.tcp(), "/api/logs", &token).await;
    assert_eq!(texts_of(&tail), ["whole"]);
    let cursor = tail["cursor"].as_str().unwrap();
    let (_, _, body) = get(
        &node.tcp(),
        &format!("/api/logs?after={cursor}&wait=60"),
        &token,
    )
    .await;
    assert!(body["lines"].as_array().unwrap().is_empty());
    assert_eq!(body["cursor"], cursor);
}

#[tokio::test]
async fn a_device_revoked_while_it_waits_gets_nothing() {
    let node = Node::timed(Duration::from_secs(10), Duration::from_millis(20));
    let (device, token) = node.device().await;
    let (_, _, tail) = get(&node.tcp(), "/api/logs", &token).await;
    let cursor = tail["cursor"].as_str().unwrap().to_owned();
    let app = node.tcp();
    let waiting = tokio::spawn(async move {
        get(&app, &format!("/api/logs?after={cursor}&wait=10"), &token).await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    node.revoke(&device);
    append(&node.log, "for nobody\n");
    let started = Instant::now();
    let (status, _, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.get("lines").is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_device_holds_at_most_four_reads_and_the_places_come_back() {
    let node = Node::timed(Duration::from_millis(700), Duration::from_millis(20));
    let (_, token) = node.device().await;
    let (_, other) = node.device().await;
    let holding: Vec<_> = (0..4)
        .map(|_| {
            let (app, token) = (node.tcp(), token.clone());
            tokio::spawn(async move { get(&app, "/api/logs?wait=60", &token).await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, _, body) = get(&node.tcp(), "/api/logs?wait=60", &token).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body, json!({"detail": refusal("request.rate_limited")}));
    // Reads that do not wait are not holds, and another device has its own places.
    assert_eq!(
        get(&node.tcp(), "/api/logs", &token).await.0,
        StatusCode::OK
    );
    let (status, ..) = get(&node.tcp(), "/api/logs?wait=1", &other).await;
    assert_eq!(status, StatusCode::OK);
    for hold in holding {
        let (status, _, body) = hold.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, ..) = get(&node.tcp(), "/api/logs?wait=1", &token).await;
    assert_eq!(status, StatusCode::OK);
    assert!(node.state.logs.waits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_device_that_only_reads_is_limited_per_window_and_the_window_ends() {
    let node = Node::shaped(|logs| logs.limited(3, Duration::from_secs(3)));
    append(&node.log, &numbered(0, 2));
    let (_, token) = node.device().await;
    let (_, other) = node.device().await;
    for _ in 0..3 {
        assert_eq!(
            get(&node.tcp(), "/api/logs", &token).await.0,
            StatusCode::OK
        );
    }
    let (status, _, body) = get(&node.tcp(), "/api/logs", &token).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body, json!({"detail": refusal("request.rate_limited")}));
    // Whatever it asks counts, a wait included; another device has its own reads.
    let (status, ..) = get(&node.tcp(), "/api/logs?wait=1", &token).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        get(&node.tcp(), "/api/logs", &other).await.0,
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(3200)).await;
    assert_eq!(
        get(&node.tcp(), "/api/logs", &token).await.0,
        StatusCode::OK
    );
}

// --- GET /api/logs?source=connector --------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, watch};

use crate::control::room::{ConnectorPeer, PeerError, PeerRequest};

/// A connector linked to the node that answers each `logs.read` as `answer` says (`None`: never answers). Returns the
/// requests it got.
fn link(
    node: &Node,
    answer: impl Fn(&Value) -> Option<Value> + Send + 'static,
) -> Arc<std::sync::Mutex<Vec<(String, Value)>>> {
    let (sender, mut requests) = mpsc::channel::<PeerRequest>(8);
    let (stop, _) = watch::channel(false);
    node.state.room.attach(
        "fixture-connector",
        ConnectorPeer {
            generation: "g1".into(),
            sender,
            stop,
        },
    );
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let mut parked = Vec::new();
        while let Some(PeerRequest {
            method,
            params,
            answer: reply,
        }) = requests.recv().await
        {
            log.lock().unwrap().push((method, params.clone()));
            match (answer(&params), reply) {
                (Some(value), Some(reply)) => {
                    let _ = reply.send(Ok::<_, PeerError>(value));
                }
                (None, reply) => parked.push(reply),
                _ => {}
            }
        }
    });
    seen
}

fn page(lines: &[&str], cursor: &str, more: bool) -> Value {
    json!({
        "lines": lines.iter().enumerate().map(|(n, text)| json!({"id": format!("id{n}"), "text": text})).collect::<Vec<_>>(),
        "cursor": cursor,
        "more": more,
    })
}

#[tokio::test]
async fn the_connector_log_needs_a_paired_device_on_both_listeners_and_the_socket_stays_closed() {
    let node = Node::new();
    let asked = link(&node, |_| Some(page(&["secret"], "c1", false)));
    let (_, token) = node.device().await;
    for app in [node.tcp(), node.socket()] {
        for headers in [&[][..], &[("authorization", "Bearer not-a-token")][..]] {
            let (status, _, body) = send(
                &app,
                "GET",
                "/api/logs?source=connector",
                None,
                headers,
                None,
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert!(body.get("lines").is_none());
        }
        let (status, _, body) = get(&app, "/api/logs?source=connector", &token).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(texts_of(&body), ["secret"]);
    }
    // Nothing was asked of the connector for the callers that were refused: one request per authorised read.
    assert_eq!(asked.lock().unwrap().len(), 2);
    let (status, ..) = send(
        &node.tcp(),
        "POST",
        "/api/logs?source=connector",
        Some(&token),
        &[],
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn the_connector_is_asked_with_the_cursor_and_limit_and_its_page_is_passed_on_whole() {
    let node = Node::new();
    let asked = link(&node, |params| {
        Some(match params["after"].as_str() {
            None => page(&["a", "b"], "c1", true),
            Some(_) => page(&["c"], "c2", false),
        })
    });
    let (_, token) = node.device().await;
    let (status, headers, body) =
        get(&node.tcp(), "/api/logs?source=connector&limit=2", &token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    assert_eq!(texts_of(&body), ["a", "b"]);
    assert_eq!(
        (body["cursor"].as_str(), body["more"].as_bool()),
        (Some("c1"), Some(true))
    );
    let (_, _, body) = get(&node.tcp(), "/api/logs?source=connector&after=c1", &token).await;
    assert_eq!(texts_of(&body), ["c"]);
    {
        let asked = asked.lock().unwrap();
        assert_eq!(asked[0], ("logs.read".into(), json!({"limit": 2})));
        assert_eq!(
            asked[1],
            ("logs.read".into(), json!({"limit": 200, "after": "c1"}))
        );
        // The core's own log is a different source: it was never asked of the connector.
    }
    append(&node.log, "core line\n");
    let (_, _, body) = get(&node.tcp(), "/api/logs", &token).await;
    assert_eq!(texts_of(&body), ["core line"]);
    let (_, _, body) = get(&node.tcp(), "/api/logs?source=core", &token).await;
    assert_eq!(texts_of(&body), ["core line"]);
}

#[tokio::test]
async fn the_connector_log_without_a_connector_or_with_one_that_does_not_answer_is_unavailable() {
    let node = Node::shaped(|logs| logs.linked(Duration::from_millis(200)));
    append(&node.log, "core line\n");
    let (_, token) = node.device().await;
    // No connector linked.
    let (status, _, body) = get(&node.tcp(), "/api/logs?source=connector", &token).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body, json!({"detail": refusal("logs.unreadable")}));
    // One that never answers.
    link(&node, |_| None);
    let started = Instant::now();
    let (status, _, body) = get(&node.tcp(), "/api/logs?source=connector", &token).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body, json!({"detail": refusal("logs.unreadable")}));
    assert!(started.elapsed() < Duration::from_secs(3));
    // The core's own log is unaffected.
    assert_eq!(
        get(&node.tcp(), "/api/logs", &token).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn what_the_connector_refuses_or_answers_badly_is_mapped_and_never_passed_on() {
    let node = Node::new();
    let (_, token) = node.device().await;
    let answers = Arc::new(std::sync::Mutex::new(Value::Null));
    let current = answers.clone();
    link(&node, move |_| Some(current.lock().unwrap().clone()));
    for (answer, status, key) in [
        (
            json!({"error": {"key": "logs.cursor_expired"}}),
            StatusCode::GONE,
            "logs.cursor_expired",
        ),
        (
            json!({"error": {"key": "logs.request_invalid"}}),
            StatusCode::BAD_REQUEST,
            "logs.request_invalid",
        ),
        (
            json!({"error": {"key": "logs.unreadable"}}),
            StatusCode::SERVICE_UNAVAILABLE,
            "logs.unreadable",
        ),
        // An old connector, or any other refusal.
        (
            json!({"error": {"key": "connector.unknown-method"}}),
            StatusCode::SERVICE_UNAVAILABLE,
            "logs.unreadable",
        ),
        // Not a page.
        (
            json!({"status": "failed"}),
            StatusCode::SERVICE_UNAVAILABLE,
            "logs.unreadable",
        ),
        (
            json!({"lines": [{"id": "a"}], "cursor": "c", "more": false}),
            StatusCode::SERVICE_UNAVAILABLE,
            "logs.unreadable",
        ),
        (
            json!({"lines": [], "cursor": "x".repeat(129), "more": false}),
            StatusCode::SERVICE_UNAVAILABLE,
            "logs.unreadable",
        ),
        (
            json!({"lines": [], "cursor": "c"}),
            StatusCode::SERVICE_UNAVAILABLE,
            "logs.unreadable",
        ),
        // More lines than were asked for.
        (
            page(&["a", "b"], "c", false),
            StatusCode::SERVICE_UNAVAILABLE,
            "logs.unreadable",
        ),
    ] {
        *answers.lock().unwrap() = answer.clone();
        let (got, _, body) = get(&node.tcp(), "/api/logs?source=connector&limit=1", &token).await;
        assert_eq!(got, status, "{answer}: {body}");
        assert_eq!(body, json!({"detail": refusal(key)}), "{answer}");
    }
}

#[tokio::test]
async fn the_source_and_the_cursor_of_a_connector_read_are_checked_here() {
    let node = Node::new();
    let (_, token) = node.device().await;
    let asked = link(&node, |_| Some(page(&[], "c", false)));
    for query in [
        "source=",
        "source=desktop",
        "source=Connector",
        &format!("source=connector&after={}", "x".repeat(129)),
        "source=connector&limit=0",
        "source=connector&wait=soon",
    ] {
        let (status, _, body) = get(&node.tcp(), &format!("/api/logs?{query}"), &token).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body, json!({"detail": refusal("logs.request_invalid")}));
    }
    assert!(
        asked.lock().unwrap().is_empty(),
        "nothing invalid reaches the connector"
    );
}

#[tokio::test]
async fn a_connector_read_waits_for_a_line_and_returns_at_the_clamp() {
    let node = Node::timed(Duration::from_millis(500), Duration::from_millis(20));
    let polls = Arc::new(AtomicUsize::new(0));
    let counted = polls.clone();
    link(&node, move |params| {
        // The third look finds a line, but only when one is not asked for already.
        Some(
            if params["after"] == "c1" && counted.fetch_add(1, Ordering::SeqCst) >= 2 {
                page(&["late"], "c2", false)
            } else {
                page(&[], params["after"].as_str().unwrap_or("start"), false)
            },
        )
    });
    let (_, token) = node.device().await;
    let started = Instant::now();
    let (_, _, body) = get(
        &node.tcp(),
        "/api/logs?source=connector&after=c1&wait=3600",
        &token,
    )
    .await;
    assert_eq!(texts_of(&body), ["late"]);
    assert!(started.elapsed() < Duration::from_millis(450));
    // Nothing comes: it returns at the clamp, the cursor unchanged.
    let started = Instant::now();
    let (status, _, body) = get(
        &node.tcp(),
        "/api/logs?source=connector&after=c9&wait=3600",
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["lines"].as_array().unwrap().is_empty());
    assert_eq!(body["cursor"], "c9");
    assert!(
        started.elapsed() >= Duration::from_millis(450)
            && started.elapsed() < Duration::from_secs(3)
    );
}

#[tokio::test]
async fn a_device_revoked_while_it_waits_for_the_connector_gets_nothing() {
    let node = Node::timed(Duration::from_secs(10), Duration::from_millis(20));
    let (device, token) = node.device().await;
    link(&node, |_| Some(page(&[], "c1", false)));
    let app = node.tcp();
    let waiting = tokio::spawn(async move {
        get(&app, "/api/logs?source=connector&after=c1&wait=10", &token).await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    node.revoke(&device);
    let (status, _, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.get("lines").is_none());
}

#[tokio::test]
async fn the_limits_are_the_same_for_both_sources() {
    let node =
        Node::shaped(|logs| logs.timed(Duration::from_millis(700), Duration::from_millis(20)));
    link(&node, |_| Some(page(&[], "c1", false)));
    let (_, token) = node.device().await;
    // Four holds in all, whichever source they read; the fifth is refused.
    let holding: Vec<_> = (0..4)
        .map(|n| {
            let (app, token) = (node.tcp(), token.clone());
            let source = if n % 2 == 0 { "connector" } else { "core" };
            tokio::spawn(async move {
                get(&app, &format!("/api/logs?source={source}&wait=60"), &token).await
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, _, body) = get(&node.tcp(), "/api/logs?source=connector&wait=60", &token).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body, json!({"detail": refusal("request.rate_limited")}));
    for hold in holding {
        assert_eq!(hold.await.unwrap().0, StatusCode::OK);
    }
    assert!(node.state.logs.waits.lock().unwrap().is_empty());
    // And the per-window bound counts connector reads.
    let node = Node::shaped(|logs| logs.limited(2, Duration::from_secs(5)));
    link(&node, |_| Some(page(&[], "c1", false)));
    let (_, token) = node.device().await;
    for _ in 0..2 {
        assert_eq!(
            get(&node.tcp(), "/api/logs?source=connector", &token)
                .await
                .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        get(&node.tcp(), "/api/logs?source=connector", &token)
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
}
