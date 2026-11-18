//! Handled errors: what is read from an event, what is sent, how often, and never while the switch is off.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};

use chrono::{Days, NaiveDate};
use tracing::{event, Level};
use tracing_subscriber::layer::SubscriberExt;
use wiremock::{Request, ResponseTemplate};

use super::*;
use crate::crash::handled::{event as handled_event, queueing, Handled, Outcome, Worker};
use crate::crash::HANDLED_TAGS;
use crate::error::CoreError;

const SECRET: &str = "canary-secret-7f3a";
const WORDS: &str = "canary words 7f3a";
const HOME: &str = "/home/canary-user-7f3a";
const MACHINE: &str = "0123456789abcdef0123456789abcdef";

// Codes of the core's own list (rust/error.rs): only these leave.
const A: &str = "core.port-in-use";
const B: &str = "core.start-failed";
const C: &str = "core.already-running";
const OTHER: &str = "core.crashed";

/// What a subscriber with only the handled-errors layer queues while `emit` logs, and how many events had no code of the core's.
fn queued(f: &Fixture, capacity: usize, emit: impl FnOnce()) -> (Vec<Handled>, u64) {
    // Its own counter for this call (leaked: the layer wants a `'static` one), so that tests running side by side do not
    // count each other's events.
    let counted: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
    let (queue, waiting): (_, Receiver<Handled>) = sync_channel(capacity);
    let subscriber =
        tracing_subscriber::registry().with(queueing(f.data.clone(), queue, counted, || false));
    tracing::subscriber::with_default(subscriber, emit);
    (
        waiting.try_iter().collect(),
        counted.load(Ordering::Relaxed),
    )
}

fn kinds(events: &[Handled]) -> Vec<&str> {
    events.iter().map(|event| event.kind).collect()
}

fn error_of(kind: &'static str) -> Handled {
    Handled {
        kind,
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
            kind: error.kind,
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
    let (events, unreported) = queued(&f, 16, || {
        event!(target: "sidevoice_core::server::call", Level::ERROR, error.type = A, "ours");
        event!(target: "sidevoice_core", Level::ERROR, error.type = B, "the crate itself");
        event!(target: "sidevoice_platform::fs", Level::ERROR, error.type = C, "ours too");
        // The panic hook's own, and whatever is below them.
        event!(target: "panic", Level::ERROR, error.type = OTHER, "the panic");
        event!(target: "panic::deeper", Level::ERROR, error.type = OTHER, "below the panic");
        event!(target: "crash_backtrace", Level::ERROR, error.type = OTHER, "the backtrace");
        event!(target: "crash_backtrace::deeper", Level::ERROR, error.type = OTHER, "below it");
        // Not ours: a dependency, another piece, a webview, a lookalike that only shares a prefix.
        event!(target: "reqwest::connect", Level::ERROR, error.type = OTHER, "a dependency");
        event!(target: "sidevoice_connector", Level::ERROR, error.type = OTHER, "another piece");
        event!(target: "webview", Level::ERROR, error.type = OTHER, "a webview");
        event!(target: "sidevoice_corevoice", Level::ERROR, error.type = OTHER, "a lookalike");
        event!(target: "panic_x", Level::ERROR, error.type = OTHER, "another lookalike");
        // Ours, but not an error.
        event!(target: "sidevoice_core::x", Level::WARN, error.type = OTHER, "a warning");
        event!(target: "sidevoice_core::x", Level::INFO, error.type = OTHER, "an info");
        event!(target: "sidevoice_core::x", Level::DEBUG, error.type = OTHER, "a debug");
        event!(target: "sidevoice_core::x", Level::TRACE, error.type = OTHER, "a trace");
    });
    assert_eq!(kinds(&events), [A, B, C]);
    assert_eq!(unreported, 0, "the filter comes before the count");
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
            kind: "core.port-in-use",
            // The callsite's, not what a field claims.
            file: "rust/crash/tests/handled.rs".to_owned(),
            line,
        }]
    );
}

#[test]
fn an_event_without_a_code_of_the_cores_is_counted_and_never_taken() {
    let f = fixture();
    let (events, unreported) = queued(&f, 16, || {
        event!(target: "sidevoice_core::x", Level::ERROR, "no field at all");
        event!(target: "sidevoice_core::x", Level::ERROR, reason = A, "another field");
        // The code only counts when it is a string.
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = ?A, "recorded with Debug");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = %A, "recorded with Display");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = 7, "a number");
        // A value of any other kind, well formed or not: a sentence, a path, an address, a host name, a word, a code of
        // another piece or one nobody wrote.
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "a sentence with spaces", "text");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = HOME, "a path");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "192.168.1.10", "an address");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "my-laptop.local", "a host name");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.alice-smith", "a name in the shape of a code");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.not-one-of-ours", "a shape-valid code nobody wrote");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "engine.provider-unreachable", "another piece's code");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "", "empty");
        // Matched whole, nothing normalised.
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "Core.Port-In-Use", "capitals");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.port-in-use ", "a space after");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.port-in-use\n", "a line after");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.port-in-use.x", "a longer one");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = A, "the one that is");
    });
    assert_eq!(kinds(&events), [A]);
    assert_eq!(unreported, 17);
}

#[test]
fn every_code_the_core_names_is_a_code_that_leaves() {
    let f = fixture();
    let (events, unreported) = queued(&f, 128, || {
        for code in CoreError::codes() {
            event!(target: "sidevoice_core::x", Level::ERROR, error.type = code, "failed");
        }
    });
    let codes: Vec<&str> = CoreError::codes().collect();
    assert!(codes.len() > 40, "the list was not read");
    assert_eq!(
        kinds(&events),
        codes,
        "the enum's codes, the refusals' and their fallback"
    );
    assert_eq!(unreported, 0);
    // The refusals reach the log as the codes of their table, whatever key a peer sent.
    for error in [
        CoreError::Refused("room.focus_changed"),
        CoreError::Refused("a key nobody listed"),
        CoreError::StateSaveFailed,
    ] {
        assert!(codes.contains(&error.code()), "{}", error.code());
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
    let emit = || event!(target: "sidevoice_core::x", Level::ERROR, error.type = A, "failed");
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
    let emit = || event!(target: "sidevoice_core::x", Level::ERROR, error.type = A, "failed");
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
            event!(target: "sidevoice_core::x", Level::ERROR, error.type = A, n, "failed");
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
        ..error_of(A)
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

    assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Sent);
    assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Capped);
    assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Capped);
    assert_eq!(handle(&mut worker, error_of(B), day).await, Outcome::Sent);
    assert_eq!(requests().await, 2, "one per type");

    assert_eq!(
        handle(&mut worker, error_of(A), next).await,
        Outcome::Sent,
        "tomorrow"
    );
    assert_eq!(
        handle(&mut worker, error_of(A), next).await,
        Outcome::Capped
    );
    assert_eq!(requests().await, 3);

    // The clock went back to a day the list is not for: a new day, and the list is that day's from now on.
    assert_eq!(
        handle(&mut worker, error_of(A), before).await,
        Outcome::Sent,
        "clock back"
    );
    assert_eq!(
        handle(&mut worker, error_of(A), before).await,
        Outcome::Capped
    );
    assert_eq!(
        handle(&mut worker, error_of(B), before).await,
        Outcome::Sent
    );
    assert_eq!(requests().await, 5);
    assert_eq!(
        handle(&mut worker, error_of(A), day).await,
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
        assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Sent);
        assert_eq!(handle(&mut worker, error_of(B), day).await, Outcome::Sent);
    }
    let file = f.data.join("handled-errors.json");
    let value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(
        keys(&value),
        ["day", "sent"],
        "codes and the day, nothing else"
    );
    assert_eq!(value["day"], day.to_string().as_str());
    assert_eq!(value["sent"], json!([A, B]));
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
    assert_eq!(handle(&mut again, error_of(A), day).await, Outcome::Capped);
    assert_eq!(handle(&mut again, error_of(C), day).await, Outcome::Sent);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);

    // A file for another day (earlier or later), a file that is not ours, and one that is not a list: an empty list.
    for kept in [
        json!({"day": "2001-01-01", "sent": [A]}),
        json!({"day": "2999-01-01", "sent": [A]}),
        json!({"day": "not a day", "sent": [A]}),
        json!([A]),
    ] {
        Files::write_json(&file, &kept).unwrap();
        let mut fresh = worker(&context, &server);
        assert_eq!(
            handle(&mut fresh, error_of(A), day).await,
            Outcome::Sent,
            "{kept}"
        );
    }
    Files::write_private(&file, b"not json").unwrap();
    let mut fresh = worker(&context, &server);
    assert_eq!(handle(&mut fresh, error_of(A), day).await, Outcome::Sent);
    let value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(value["sent"], json!([A]), "replaced by a good one");
}

#[tokio::test]
async fn a_file_with_things_that_are_not_the_cores_codes_is_read_as_its_codes_only() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let day = today();
    Files::write_json(
        &f.data.join("handled-errors.json"),
        &json!({"day": day.to_string(), "sent": [
            "not a code", HOME, A, A, "Core.Start-Failed", 7, "core.not-one-of-ours", "192.168.1.10", C
        ]}),
    )
    .unwrap();
    let mut worker = worker(&context, &server);
    assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Capped);
    assert_eq!(handle(&mut worker, error_of(C), day).await, Outcome::Capped);
    assert_eq!(
        handle(&mut worker, error_of(B), day).await,
        Outcome::Sent,
        "what is not one of the codes says nothing about it"
    );
    let value: Value =
        serde_json::from_slice(&fs::read(f.data.join("handled-errors.json")).unwrap()).unwrap();
    assert_eq!(value["sent"], json!([A, C, B]), "the codes, once each");
}

#[tokio::test]
async fn the_days_list_is_bounded_by_the_closed_list_of_codes() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let day = today();
    // Every code the core has, listed for today (written, not sent one by one: every send is a durable write), among a
    // hundred things that are not codes.
    let all: Vec<&str> = CoreError::codes().collect();
    let mut sent: Vec<String> = (0..100).map(|n| format!("core.made-up-{n}")).collect();
    sent.extend(all.iter().map(|code| (*code).to_owned()));
    let file = f.data.join("handled-errors.json");
    Files::write_json(&file, &json!({"day": day.to_string(), "sent": sent})).unwrap();
    // The file is read, and holds all of them: nothing more can be sent today, whatever comes.
    let mut worker = Worker::new(&context, reqwest::Client::new(), dsn(&server));
    for code in &all {
        assert_eq!(
            handle(&mut worker, error_of(code), day).await,
            Outcome::Capped,
            "{code}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    let tomorrow = day.checked_add_days(Days::new(1)).unwrap();
    assert_eq!(
        handle(&mut worker, error_of(all[0]), tomorrow).await,
        Outcome::Sent
    );
    let value: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(value["sent"], json!([all[0]]), "tomorrow's list");

    // A file past the size the core reads is not read at all.
    let huge: Vec<String> = (0..3000).map(|n| format!("core.made-up-{n}")).collect();
    Files::write_json(&file, &json!({"day": day.to_string(), "sent": huge})).unwrap();
    assert!(fs::metadata(&file).unwrap().len() > 8 * 1024);
    let mut fresh = Worker::new(&context, reqwest::Client::new(), dsn(&server));
    assert_eq!(handle(&mut fresh, error_of(A), day).await, Outcome::Sent);
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
        assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Off);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(
        names(&f.data),
        ["crash-reports.json"],
        "no id and no list were made"
    );

    // On: it goes, and turning it off erases the list with the id, and what the worker remembered.
    set_enabled(&f.data, true).unwrap();
    assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Sent);
    assert!(f.data.join("handled-errors.json").exists());
    assert!(f.data.join("crash-machine-id.json").exists());
    set_enabled(&f.data, false).unwrap();
    assert_eq!(names(&f.data), ["crash-reports.json"]);
    assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Off);
    set_enabled(&f.data, true).unwrap();
    assert_eq!(
        handle(&mut worker, error_of(A), day).await,
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
        handle(&mut worker, error_of(A), today()).await,
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
    assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Unsent);
    assert_eq!(handle(&mut worker, error_of(B), day).await, Outcome::Quiet);
    assert_eq!(handle(&mut worker, error_of(A), day).await, Outcome::Quiet);
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
    assert_eq!(handle(&mut later, error_of(A), day).await, Outcome::Sent);

    // A server that is not there is the same.
    let mut lost = Worker::new(&context, reqwest::Client::new(), closed_dsn());
    assert_eq!(
        handle(&mut lost, error_of(OTHER), day).await,
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

#[tokio::test]
async fn a_well_formed_code_that_is_not_the_cores_never_reaches_the_server() {
    let f = fixture();
    let server = envelope_server(200).await;
    let context = context(&f);
    let (events, unreported) = queued(&f, 16, || {
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "192.168.1.10", "an address");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "my-laptop.local", "a host name");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = "core.not-one-of-ours", "a code nobody wrote");
        event!(target: "sidevoice_core::x", Level::ERROR, error.type = A, "a code of the core's");
    });
    assert_eq!(unreported, 3);
    let mut worker = worker(&context, &server);
    for error in events {
        assert_eq!(handle(&mut worker, error, today()).await, Outcome::Sent);
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "only the code of the core's");
    let body = String::from_utf8_lossy(&requests[0].body).into_owned();
    assert!(body.contains(A), "{body}");
    for other in ["192.168.1.10", "my-laptop.local", "not-one-of-ours"] {
        assert!(!body.contains(other), "{other} in {body}");
    }
}
