//! Handled errors: what is read from an event, what is sent, how often, and never while the switch is off.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};

use chrono::{Days, NaiveDate};
use tracing::{event, Level};
use tracing_subscriber::layer::SubscriberExt;
use wiremock::{Request, ResponseTemplate};

use super::*;
use crate::crash::handled::{
    event as handled_event, is_code, queueing, Handled, Outcome, Worker, MAX_TYPES,
};
use crate::crash::HANDLED_TAGS;
use crate::error::CoreError;

const SECRET: &str = "canary-secret-7f3a";
const WORDS: &str = "canary words 7f3a";
const HOME: &str = "/home/canary-user-7f3a";
const MACHINE: &str = "0123456789abcdef0123456789abcdef";

/// What a subscriber with only the handled-errors layer queues while `emit` logs, and how many events had no usable code.
fn queued(f: &Fixture, capacity: usize, emit: impl FnOnce()) -> (Vec<Handled>, u64) {
    static COUNTED: AtomicU64 = AtomicU64::new(0);
    // Each test takes the difference, so another test's events do not count.
    let before = COUNTED.load(Ordering::Relaxed);
    let (queue, waiting): (_, Receiver<Handled>) = sync_channel(capacity);
    let subscriber =
        tracing_subscriber::registry().with(queueing(f.data.clone(), queue, &COUNTED, || false));
    tracing::subscriber::with_default(subscriber, emit);
    (
        waiting.try_iter().collect(),
        COUNTED.load(Ordering::Relaxed) - before,
    )
}

fn kinds(events: &[Handled]) -> Vec<&str> {
    events.iter().map(|event| event.kind.as_str()).collect()
}

fn error_of(kind: &str) -> Handled {
    Handled {
        kind: kind.to_owned(),
        file: "rust/server/x.rs".to_owned(),
        line: 7,
    }
}

fn today() -> NaiveDate {
    chrono::Utc::now().date_naive()
}

fn worker<'a>(context: &'a Context, server: &MockServer) -> Worker<'a> {
    Worker::new(context, reqwest::Client::new(), dsn(server))
}

/// `handle`, waiting out a send that found the process's two slots taken by other tests.
async fn handle(worker: &mut Worker<'_>, error: Handled, day: NaiveDate) -> Outcome {
    for _ in 0..200 {
        let again = Handled {
            kind: error.kind.clone(),
            file: error.file.clone(),
            line: error.line,
        };
        match worker.handle(again, day).await {
            Outcome::Busy => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            outcome => return outcome,
        }
    }
    Outcome::Busy
}

// Which events.

#[test]
fn only_error_events_of_our_own_targets_are_taken_and_never_the_panic_hooks() {
    let f = fixture();
    let (events, untyped) = queued(&f, 16, || {
        event!(target: "sidevoice_core::server::call", Level::ERROR, error.type = "core.a", "ours");
        event!(target: "sidevoice_core", Level::ERROR, error.type = "core.root", "the crate itself");
        event!(target: "sidevoice_platform::fs", Level::ERROR, error.type = "platform.b", "ours too");
        // The panic hook's own, and whatever is below them.
        event!(target: "panic", Level::ERROR, error.type = "core.p1", "the panic");
        event!(target: "panic::deeper", Level::ERROR, error.type = "core.p2", "below the panic");
        event!(target: "crash_backtrace", Level::ERROR, error.type = "core.p3", "the backtrace");
        event!(target: "crash_backtrace::deeper", Level::ERROR, error.type = "core.p4", "below it");
        // Not ours: a dependency, another piece, a webview, a lookalike that only shares a prefix.
        event!(target: "reqwest::connect", Level::ERROR, error.type = "core.d1", "a dependency");
        event!(target: "sidevoice_connector", Level::ERROR, error.type = "core.d2", "another piece");
        event!(target: "webview", Level::ERROR, error.type = "core.d3", "a webview");
        event!(target: "sidevoice_corevoice", Level::ERROR, error.type = "core.d4", "a lookalike");
        event!(target: "panic_x", Level::ERROR, error.type = "core.d5", "another lookalike");
        // Ours, but not an error.
        event!(target: "sidevoice_core::x", Level::WARN, error.type = "core.w", "a warning");
        event!(target: "sidevoice_core::x", Level::INFO, error.type = "core.i", "an info");
        event!(target: "sidevoice_core::x", Level::DEBUG, error.type = "core.g", "a debug");
        event!(target: "sidevoice_core::x", Level::TRACE, error.type = "core.t", "a trace");
    });
    assert_eq!(kinds(&events), ["core.a", "core.root", "platform.b"]);
    assert_eq!(untyped, 0, "the filter comes before the count");
}

#[test]
fn an_event_is_read_for_its_code_and_its_place_and_for_nothing_else() {
    let f = fixture();
    let line = line!() + 2;
    let (events, _) = queued(&f, 16, || {
        event!(
            target: "sidevoice_core::server::call",
            Level::ERROR,
            error.type = "core.port-in-use",
            code.file.path = "/home/canary-user-7f3a/src/lie.rs",
            code.line.number = 99_999,
            secret = SECRET,
            path = HOME,
            words = %WORDS,
            detail = ?WORDS,
            kind = "canary",
            "the port canary words 7f3a was taken by canary-secret-7f3a"
        );
    });
    assert_eq!(
        events,
        [Handled {
            kind: "core.port-in-use".to_owned(),
            // The callsite's, not what a field claims.
            file: "rust/crash/tests/handled.rs".to_owned(),
            line,
        }]
    );
}

#[test]
fn an_event_without_a_usable_code_is_counted_and_never_taken() {
    let f = fixture();
    let (events, untyped) = queued(&f, 16, || {
        event!(target: "sidevoice_core::x", Level::ERROR, "no field at all");
        event!(target: "sidevoice_core::x", Level::ERROR, reason = "core.a", "another field");
        // The code only counts when it is a string.
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = ?"core.debug", "recorded with Debug");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = %"core.display", "recorded with Display");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = 7, "a number");
        // And when it is a code: not a sentence, a path, a name or anything long.
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "a sentence with spaces", "text");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = HOME, "a path");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "Core.Upper", "capitals");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "nodot", "no component");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "", "empty");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.a_b", "an underscore");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.-a", "a dash after the dot");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core..a", "two dots");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "long");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.fine", "the one that is");
    });
    assert_eq!(kinds(&events), ["core.fine"]);
    assert_eq!(untyped, 14);
    for code in ["core.port-in-use", "platform.x9", "a.b.c-d"] {
        assert!(is_code(code), "{code}");
    }
    // The codes the core itself names (rust/error.rs) are all codes that leave, the refusals included.
    for error in [
        CoreError::StartFailed,
        CoreError::StateSaveFailed,
        CoreError::CrashReportFailed,
        CoreError::Refused("room.focus_changed"),
        CoreError::Refused("a key nobody listed"),
    ] {
        assert!(is_code(error.code()), "{}", error.code());
    }
}

#[test]
fn the_place_is_a_short_relative_path_or_unknown() {
    use crate::crash::record::relative_path;
    assert_eq!(relative_path("rust/server/call.rs"), "rust/server/call.rs");
    assert_eq!(
        relative_path("platform\\src\\fs.rs"),
        "platform/src/fs.rs",
        "a Windows build"
    );
    for path in [
        "",
        "/home/canary-user-7f3a/.cargo/registry/src/x/a.rs",
        "C:\\Users\\canary-user-7f3a\\a.rs",
        "~/a.rs",
        "../a.rs",
        "a b.rs",
    ] {
        assert_eq!(relative_path(path), "unknown", "{path}");
    }
}

// The layer: the switch, the hook and the queue.

#[test]
fn nothing_is_queued_while_the_switch_is_off_or_cannot_be_read() {
    let f = fixture();
    let emit =
        || event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.a", "failed");
    assert_eq!(queued(&f, 16, emit).0.len(), 1, "on by default");
    Files::write_json(&f.data.join("crash-reports.json"), &json!({"send": false})).unwrap();
    assert!(queued(&f, 16, emit).0.is_empty(), "off");
    Files::write_private(&f.data.join("crash-reports.json"), b"not json").unwrap();
    assert!(queued(&f, 16, emit).0.is_empty(), "unknown");
    Files::write_private(&f.data.join("crash-reports.json"), br#"{"send": "yes"}"#).unwrap();
    assert!(queued(&f, 16, emit).0.is_empty(), "not understood");
    Files::write_json(&f.data.join("crash-reports.json"), &json!({"send": true})).unwrap();
    assert_eq!(queued(&f, 16, emit).0.len(), 1, "on");
    set_enabled(&f.data, false).unwrap();
    assert!(queued(&f, 16, emit).0.is_empty(), "off again");
}

#[test]
fn a_sending_thread_and_the_panic_hook_never_feed_the_queue() {
    let f = fixture();
    let emit =
        || event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.a", "failed");
    SENDING.set(true);
    let from_a_sender = queued(&f, 16, emit).0;
    SENDING.set(false);
    assert!(from_a_sender.is_empty());
    static COUNTED: AtomicU64 = AtomicU64::new(0);
    let (queue, waiting) = sync_channel(16);
    let subscriber =
        tracing_subscriber::registry().with(queueing(f.data.clone(), queue, &COUNTED, || true));
    tracing::subscriber::with_default(subscriber, emit);
    assert!(waiting.try_iter().next().is_none(), "inside the hook");
}

#[test]
fn a_full_queue_drops_the_event_and_the_logging_thread_never_waits() {
    let f = fixture();
    let (events, _) = queued(&f, 2, || {
        for n in 0..50 {
            event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.a", n, "failed");
        }
    });
    assert_eq!(events.len(), 2);
}

// The event.

#[test]
fn the_event_carries_the_closed_field_list_and_no_text_of_the_event() {
    let f = fixture();
    let (events, _) = queued(&f, 16, || {
        event!(
            target: "sidevoice_core::server::call",
            Level::ERROR,
            error.type = "core.port-in-use",
            secret = SECRET,
            path = HOME,
            words = %WORDS,
            "the port canary words 7f3a was taken by canary-secret-7f3a at /home/canary-user-7f3a"
        );
    });
    let value = serde_json::to_value(handled_event(&context(&f), &events[0], MACHINE)).unwrap();
    assert_eq!(
        keys(&value),
        [
            "event_id",
            "exception",
            "platform",
            "release",
            "tags",
            "timestamp"
        ],
        "no extra, breadcrumb, context, user, request, message or host name, and the default level: {value}"
    );
    // Sentry's default level is error, so the SDK's type leaves it out: the field is there only when it is not the default.
    assert!(value.get("level").is_none(), "{value}");
    assert_eq!(
        value["release"],
        format!("sidevoice-core@{}", env!("CARGO_PKG_VERSION")).as_str()
    );
    assert_eq!(keys(&value["tags"]), {
        let mut tags = HANDLED_TAGS.to_vec();
        tags.sort_unstable();
        tags
    });
    assert_eq!(value["tags"]["service.name"], "sidevoice-core");
    assert_eq!(value["tags"]["os.type"], "linux");
    assert_eq!(value["tags"]["os.name"], "Linux");
    assert_eq!(value["tags"]["host.arch"], "amd64");
    assert_eq!(value["tags"]["installation.id"], MACHINE);
    let exception = &value["exception"]["values"];
    assert_eq!(exception.as_array().unwrap().len(), 1);
    assert_eq!(keys(&exception[0]), ["stacktrace", "type"], "no message");
    assert_eq!(exception[0]["type"], "core.port-in-use");
    let frames = exception[0]["stacktrace"]["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(keys(&frames[0]), ["filename", "lineno"]);
    assert_eq!(frames[0]["filename"], "rust/crash/tests/handled.rs");
    assert!(frames[0]["lineno"].as_u64().unwrap() > 0);
    // Random per event: nothing links two of them but the machine id.
    let other = serde_json::to_value(handled_event(&context(&f), &events[0], MACHINE)).unwrap();
    assert_ne!(value["event_id"], other["event_id"]);

    let json = value.to_string();
    for canary in ["canary", "/home/"] {
        assert!(!json.contains(canary), "{canary} in {json}");
    }
    use crate::platform::user::{User, UserId};
    let host = UserId::host_name();
    if host.len() >= 6 {
        assert!(!json.contains(&host), "the host name is in {json}");
    }
}

#[test]
fn a_place_that_is_not_ours_to_say_is_unknown_in_the_event() {
    let f = fixture();
    let place = Handled {
        file: crate::crash::record::relative_path(
            "/home/canary-user-7f3a/.cargo/registry/src/x/a.rs",
        ),
        ..error_of("core.a")
    };
    let json = serde_json::to_string(&handled_event(&context(&f), &place, MACHINE)).unwrap();
    assert!(json.contains("\"filename\":\"unknown\""), "{json}");
    assert!(
        !json.contains("canary") && !json.contains("/home/"),
        "{json}"
    );
}

// The sender.

#[tokio::test]
async fn an_error_is_sent_once_a_day_per_type_and_a_new_day_or_a_clock_gone_back_is_a_new_day() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let mut worker = worker(&context, &server);
    let day = today();
    let next = day.checked_add_days(Days::new(1)).unwrap();
    let before = day.checked_sub_days(Days::new(1)).unwrap();
    let requests = || async { server.received_requests().await.unwrap().len() };

    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Sent
    );
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Capped
    );
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Capped
    );
    assert_eq!(
        handle(&mut worker, error_of("core.b"), day).await,
        Outcome::Sent
    );
    assert_eq!(requests().await, 2, "one per type");

    assert_eq!(
        handle(&mut worker, error_of("core.a"), next).await,
        Outcome::Sent,
        "tomorrow"
    );
    assert_eq!(
        handle(&mut worker, error_of("core.a"), next).await,
        Outcome::Capped
    );
    assert_eq!(requests().await, 3);

    // The clock went back to a day the list is not for: a new day, and the list is that day's from now on.
    assert_eq!(
        handle(&mut worker, error_of("core.a"), before).await,
        Outcome::Sent,
        "clock back"
    );
    assert_eq!(
        handle(&mut worker, error_of("core.a"), before).await,
        Outcome::Capped
    );
    assert_eq!(
        handle(&mut worker, error_of("core.b"), before).await,
        Outcome::Sent
    );
    assert_eq!(requests().await, 5);
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Sent,
        "and forward again"
    );
    assert_eq!(requests().await, 6);
}

#[tokio::test]
async fn the_days_list_is_a_private_file_that_survives_a_restart_for_that_day_only() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let day = today();
    {
        let mut worker = worker(&context, &server);
        assert_eq!(
            handle(&mut worker, error_of("core.a"), day).await,
            Outcome::Sent
        );
        assert_eq!(
            handle(&mut worker, error_of("core.b"), day).await,
            Outcome::Sent
        );
    }
    let file = f.data.join("handled-errors.json");
    let value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(
        keys(&value),
        ["day", "sent"],
        "codes and the day, nothing else"
    );
    assert_eq!(value["day"], day.to_string().as_str());
    assert_eq!(value["sent"], json!(["core.a", "core.b"]));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o077,
            0,
            "private"
        );
    }

    // Another run, the same day: the list is read back.
    let mut again = worker(&context, &server);
    assert_eq!(
        handle(&mut again, error_of("core.a"), day).await,
        Outcome::Capped
    );
    assert_eq!(
        handle(&mut again, error_of("core.c"), day).await,
        Outcome::Sent
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 3);

    // A file for another day (earlier or later), a file that is not ours, and one that is not a list: an empty list.
    for kept in [
        json!({"day": "2001-01-01", "sent": ["core.a"]}),
        json!({"day": "2999-01-01", "sent": ["core.a"]}),
        json!({"day": "not a day", "sent": ["core.a"]}),
        json!(["core.a"]),
    ] {
        Files::write_json(&file, &kept).unwrap();
        let mut fresh = worker(&context, &server);
        assert_eq!(
            handle(&mut fresh, error_of("core.a"), day).await,
            Outcome::Sent,
            "{kept}"
        );
    }
    Files::write_private(&file, b"not json").unwrap();
    let mut fresh = worker(&context, &server);
    assert_eq!(
        handle(&mut fresh, error_of("core.a"), day).await,
        Outcome::Sent
    );
    let value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(value["sent"], json!(["core.a"]), "replaced by a good one");
}

#[tokio::test]
async fn a_file_with_things_that_are_not_codes_is_read_as_its_codes_only() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let day = today();
    Files::write_json(
        &f.data.join("handled-errors.json"),
        &json!({"day": day.to_string(), "sent": ["not a code", HOME, "core.a", "core.a", "Core.B", 7, "core.c"]}),
    )
    .unwrap();
    let mut worker = worker(&context, &server);
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Capped
    );
    assert_eq!(
        handle(&mut worker, error_of("core.c"), day).await,
        Outcome::Capped
    );
    assert_eq!(
        handle(&mut worker, error_of("core.b"), day).await,
        Outcome::Sent,
        "what is not a code says nothing about it"
    );
    let value: Value =
        serde_json::from_slice(&fs::read(f.data.join("handled-errors.json")).unwrap()).unwrap();
    assert_eq!(value["sent"], json!(["core.a", "core.c", "core.b"]));
}

#[tokio::test]
async fn the_days_list_and_so_the_days_sends_are_bounded() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let day = today();
    // A list one short of full (written, not sent one by one: every send is a durable write).
    let almost: Vec<String> = (0..MAX_TYPES - 1).map(|n| format!("core.u{n}")).collect();
    Files::write_json(
        &f.data.join("handled-errors.json"),
        &json!({"day": day.to_string(), "sent": almost}),
    )
    .unwrap();
    let mut worker = worker(&context, &server);
    assert_eq!(
        handle(&mut worker, error_of("core.one-more"), day).await,
        Outcome::Sent,
        "room for one"
    );
    assert_eq!(
        handle(&mut worker, error_of("core.too-many"), day).await,
        Outcome::Capped,
        "a full list sends no more today, however many types come"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let value: Value =
        serde_json::from_slice(&fs::read(f.data.join("handled-errors.json")).unwrap()).unwrap();
    assert_eq!(value["sent"].as_array().unwrap().len(), MAX_TYPES);

    // A file that holds far more than the bound is cut to it when read, and is as full.
    let many: Vec<String> = (0..500).map(|n| format!("core.t{n}")).collect();
    Files::write_json(
        &f.data.join("handled-errors.json"),
        &json!({"day": day.to_string(), "sent": many}),
    )
    .unwrap();
    let mut read = Worker::new(&context, reqwest::Client::new(), dsn(&server));
    assert_eq!(
        handle(&mut read, error_of("core.t499"), day).await,
        Outcome::Capped
    );
    let tomorrow = day.checked_add_days(Days::new(1)).unwrap();
    assert_eq!(
        handle(&mut read, error_of("core.t499"), tomorrow).await,
        Outcome::Sent
    );
}

#[tokio::test]
async fn nothing_is_sent_or_made_while_the_switch_is_off_or_cannot_be_read() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let day = today();
    let mut worker = worker(&context, &server);
    for switch in [
        br#"{"send": false}"#.as_slice(),
        b"not json".as_slice(),
        br#"{"send": "yes"}"#.as_slice(),
    ] {
        Files::write_private(&f.data.join("crash-reports.json"), switch).unwrap();
        assert_eq!(
            handle(&mut worker, error_of("core.a"), day).await,
            Outcome::Off
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(
        names(&f.data),
        ["crash-reports.json"],
        "no id and no list were made"
    );

    // On: it goes, and turning it off erases the list with the id, and what the worker remembered.
    set_enabled(&f.data, true).unwrap();
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Sent
    );
    assert!(f.data.join("handled-errors.json").exists());
    assert!(f.data.join("crash-machine-id.json").exists());
    set_enabled(&f.data, false).unwrap();
    assert_eq!(names(&f.data), ["crash-reports.json"]);
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Off
    );
    set_enabled(&f.data, true).unwrap();
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Sent,
        "a new start: what was forgotten is not remembered"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_switch_turned_off_while_the_post_is_on_the_wire_leaves_no_list_behind() {
    let f = fixture();
    let server = wiremock::MockServer::start().await;
    let data = f.data.clone();
    wiremock::Mock::given(method("POST"))
        .and(path("/api/42/envelope/"))
        .respond_with(move |_: &Request| {
            Files::write_json(&data.join("crash-reports.json"), &json!({"send": false})).unwrap();
            ResponseTemplate::new(200)
        })
        .mount(&server)
        .await;
    let context = context(&f);
    let mut worker = worker(&context, &server);
    assert_eq!(
        handle(&mut worker, error_of("core.a"), today()).await,
        Outcome::Sent
    );
    assert!(!f.data.join("handled-errors.json").exists());
    assert!(!enabled(&f.data));
}

#[tokio::test]
async fn a_failed_send_is_not_counted_and_the_network_is_left_alone_for_a_while() {
    let f = fixture();
    let failing = envelope_server(500).await;
    let context = context(&f);
    let day = today();
    let mut worker = worker(&context, &failing);
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Unsent
    );
    assert_eq!(
        handle(&mut worker, error_of("core.b"), day).await,
        Outcome::Quiet
    );
    assert_eq!(
        handle(&mut worker, error_of("core.a"), day).await,
        Outcome::Quiet
    );
    assert_eq!(
        failing.received_requests().await.unwrap().len(),
        1,
        "one try, then quiet"
    );
    assert!(
        !f.data.join("handled-errors.json").exists(),
        "nothing was sent"
    );

    // The same type, on a later run with the network back: it was never counted.
    let working = envelope_server(200).await;
    let mut later = Worker::new(&context, reqwest::Client::new(), dsn(&working));
    assert_eq!(
        handle(&mut later, error_of("core.a"), day).await,
        Outcome::Sent
    );

    // A server that is not there is the same.
    let mut lost = Worker::new(&context, reqwest::Client::new(), closed_dsn());
    assert_eq!(
        handle(&mut lost, error_of("core.z"), day).await,
        Outcome::Unsent
    );
}

#[tokio::test]
async fn what_reaches_the_server_is_the_closed_event_under_the_machine_id() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let (events, _) = queued(&f, 16, || {
        event!(
            target: "sidevoice_core::server::call",
            Level::ERROR,
            error.type = "core.port-in-use",
            secret = SECRET,
            path = HOME,
            words = %WORDS,
            "the port canary words 7f3a was taken by canary-secret-7f3a at /home/canary-user-7f3a"
        );
    });
    let mut worker = worker(&context, &server);
    let sent = handle(&mut worker, events.into_iter().next().unwrap(), today()).await;
    assert_eq!(sent, Outcome::Sent);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8_lossy(&requests[0].body).into_owned();
    assert!(requests[0].headers.contains_key("x-sentry-auth"));
    assert_eq!(
        requests[0].headers["content-type"],
        "application/x-sentry-envelope"
    );
    assert!(body.contains("core.port-in-use"), "{body}");
    // The machine id is the one the crash reports use.
    assert!(
        body.contains(&settings::machine_id(&f.data).unwrap()),
        "{body}"
    );
    for canary in [
        "canary",
        "/home/",
        "breadcrumbs",
        "server_name",
        "\"user\"",
        "\"request\"",
    ] {
        assert!(!body.contains(canary), "{canary} in {body}");
    }
    use crate::platform::user::{User, UserId};
    let host = UserId::host_name();
    if host.len() >= 6 {
        assert!(!body.contains(&host), "the host name is in {body}");
    }
}
