//! What the core's log files must never hold. The real binary runs with `SIDEVOICE_LOG=trace` through a flow that
//! touches secrets and conversation content, then the real `core.log` and the captured stderr are searched for the
//! seeded values (`docs/logging.md`, section 10). Its own test binary and its one test, so the global subscriber
//! sees every thread. A deliberate panic carrying a canary path goes through the real subscriber, writer and hook in
//! a child process: the binary has no way to be made to panic.

mod support;

use std::fs;
use std::process::Command;

use serde_json::{json, Value};
use sidevoice_core::log_safe::IdHash;
use support::*;

const SECRET: &str = "canary-secret-7f3a";
const WORDS: &str = "canary words 7f3a";
const URL: &str = "https://u:canary-pass@x/?canary-q=1";
const THREAD: &str = "canary-thread-7f3a";
const PANIC_DIR: &str = "SIDEVOICE_CANARY_PANIC_DIR";
const SEED_HOME: &str = "canary-user-7f3a";

fn seeds(home: &str) -> Vec<String> {
    let mut seeds: Vec<String> = [
        home,
        SEED_HOME,
        SECRET,
        WORDS,
        URL,
        "canary-pass",
        "canary-q",
        THREAD,
    ]
    .map(str::to_owned)
    .into();
    // `Vec<u8>` prints as `[99, 97, ...]`.
    let decimal = |text: &str| {
        text.bytes()
            .map(|byte| byte.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    seeds.extend([decimal("canary"), decimal(SECRET)]);
    seeds
}

fn assert_clean(what: &str, text: &str, home: &str) {
    for seed in seeds(home) {
        assert!(!text.contains(&seed), "{what} holds {seed:?}:\n{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_secret_or_said_reaches_the_core_log() {
    if let Ok(dir) = std::env::var(PANIC_DIR) {
        return panic_child(&dir);
    }
    // The data directory, and so core.log, sit under the seeded home. `dirs` reads HOME on Unix, USERPROFILE on Windows.
    let root = support::private_tempdir();
    let home = root.path().join(SEED_HOME);
    let data = home.join(".sidevoice");
    support::private_dir(&home);
    support::private_dir(&data);
    let home = home.to_str().unwrap().to_owned();
    let mut core = Launch::new(data.join("core"))
        .env("SIDEVOICE_LOG", "trace")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .start();

    // A device named by a person, a bearer value as a header and as a subprotocol, both refused.
    let token = core.pair_local(WORDS).await;
    let refused = core
        .get("/api/presentation")
        .header(
            "sec-websocket-protocol",
            &format!("sidevoice.token.{SECRET}"),
        )
        .token(SECRET)
        .send()
        .await;
    assert_eq!(refused.status, 401, "{refused:?}");
    drop(core.open_call(SECRET).await);

    // A call, a conversation named by the agent, a turn id that is a sentence, and a turn that is said.
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (mut peer, binding) = connector_with_binding(&core, "canary", THREAD).await;
    core.select(&token, &session, THREAD).await;
    browser.start_turn(WORDS).await;
    browser
        .report(
            "voice-user-turn",
            json!({"phase": "finished", "turn_id": WORDS, "text": WORDS}),
        )
        .await;
    accept_delivery(&mut peer).await;
    let revision = core.revision(&token, &session).await;
    browser.start_turn("turn-ok-1").await;
    browser
        .report(
            "voice-user-turn",
            json!({"phase": "finished", "turn_id": "turn-ok-1", "text": WORDS}),
        )
        .await;
    accept_delivery(&mut peer).await;
    let reply = publish(
        &mut peer,
        &binding,
        &session,
        revision + 1,
        2,
        "canary-say",
        WORDS,
    )
    .await;
    assert_eq!(reply["status"], "queued", "{reply}");
    // What the page says of its own failure, in free text.
    let reported = core
        .post(
            "/api/presentation/client-error",
            json!({"session_id": session, "kind": WORDS, "message": format!("{WORDS} {URL} {home}")}),
        )
        .token(&token)
        .send()
        .await;
    assert_eq!(reported.status, 200, "{reported:?}");

    drop(browser);
    peer.disconnect();
    // The refusals that log: more failed attempts than the budget, and more connections than the limit.
    let mut limited = false;
    for _ in 0..400 {
        limited |= core
            .get("/api/presentation")
            .token(SECRET)
            .send()
            .await
            .status
            == 429;
    }
    assert!(limited, "the attempt budget was never reached");
    let mut held = Vec::new();
    for _ in 0..300 {
        held.push(
            tokio::net::TcpStream::connect(("127.0.0.1", core.port))
                .await
                .unwrap(),
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    drop(held);
    assert_eq!(core.stop(), 0);

    let log = fs::read_to_string(data.join("core.log")).expect("the core's own log");
    let stderr = core.log();
    assert_clean("core.log", &log, &home);
    assert_clean("stderr", &stderr, &home);
    // Positive controls: the flow was logged, and a hostile id became `invalid`.
    for expected in [
        "core starting",
        "core ready",
        "call joined",
        "connector linked",
        "turn started",
        "turn=invalid",
        "turn=turn-ok-1",
        "turn finished",
        "delivery",
        "rate limit reached",
        "connection limit reached",
        "core stopped",
    ] {
        assert!(log.contains(expected), "no {expected:?} in:\n{log}");
    }
    assert!(
        log.contains(&format!("conversation={}", IdHash(THREAD))),
        "{log}"
    );
    assert!(log.contains("call{session="), "spans name the call:\n{log}");
    assert!(log.contains("client error reported"), "{log}");

    // A panic whose message carries a path: through the subscriber, so the home shows as `~`, and not chained to
    // the default hook, which would print it raw on stderr.
    let dir = root.path().join("panic");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "nothing_secret_or_said_reaches_the_core_log",
            "--nocapture",
        ])
        .env(PANIC_DIR, &dir)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("SIDEVOICE_LOG", "trace")
        .output()
        .unwrap();
    let child_stderr = String::from_utf8_lossy(&output.stderr);
    let child_stdout = String::from_utf8_lossy(&output.stdout);
    assert_clean("the panicking process's stderr", &child_stderr, &home);
    assert_clean("the panicking process's stdout", &child_stdout, &home);
    let panicked = fs::read_to_string(dir.join("core.log")).expect("the panic's log");
    assert_clean("the panic's core.log", &panicked, &home);
    assert!(
        panicked.contains("ERROR panic: panic")
            && panicked.contains("panic=\"canary path ~/project\""),
        "{panicked}"
    );
}

/// The process the panic test starts: the real subscriber, writer and hook, then a panic that names a path.
fn panic_child(dir: &str) {
    let config = sidevoice_core::runtime::Config::from_args(
        ["--data-dir", &format!("{dir}/core")].map(std::ffi::OsString::from),
    )
    .unwrap();
    support::private_dir(std::path::Path::new(dir));
    assert!(sidevoice_core::runtime::init_logging(&config).is_ok());
    let home = std::env::var("HOME").unwrap();
    let _ = std::thread::spawn(move || panic!("canary path {home}/project")).join();
    std::process::exit(3);
}
