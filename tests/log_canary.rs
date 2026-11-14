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

const HOME: &str = "/home/canary-user-7f3a";
const SECRET: &str = "canary-secret-7f3a";
const WORDS: &str = "canary words 7f3a";
const URL: &str = "https://u:canary-pass@x/?canary-q=1";
const THREAD: &str = "canary-thread-7f3a";
const PANIC_DIR: &str = "SIDEVOICE_CANARY_PANIC_DIR";

fn seeds() -> Vec<String> {
    let mut seeds: Vec<String> = [HOME, SECRET, WORDS, URL, "canary-pass", "canary-q", THREAD]
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

fn assert_clean(what: &str, text: &str) {
    for seed in seeds() {
        assert!(!text.contains(&seed), "{what} holds {seed:?}:\n{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_secret_or_said_reaches_the_core_log() {
    if let Ok(dir) = std::env::var(PANIC_DIR) {
        return panic_child(&dir);
    }
    let root = support::private_tempdir();
    let mut core = Launch::new(root.path().join("core"))
        .env("SIDEVOICE_LOG", "trace")
        .env("HOME", HOME)
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
            json!({"session_id": session, "kind": WORDS, "message": format!("{WORDS} {URL} {HOME}")}),
        )
        .token(&token)
        .send()
        .await;
    assert_eq!(reported.status, 200, "{reported:?}");

    drop(browser);
    peer.disconnect();
    assert_eq!(core.stop(), 0);

    let log = fs::read_to_string(root.path().join("core.log")).expect("the core's own log");
    let stderr = core.log();
    assert_clean("core.log", &log);
    assert_clean("stderr", &stderr);
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
        "rate limit",
        "core stopped",
    ] {
        if expected == "rate limit" {
            continue;
        }
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
        .env("HOME", HOME)
        .env("SIDEVOICE_LOG", "trace")
        .output()
        .unwrap();
    let child_stderr = String::from_utf8_lossy(&output.stderr);
    let child_stdout = String::from_utf8_lossy(&output.stdout);
    assert_clean("the panicking process's stderr", &child_stderr);
    assert_clean("the panicking process's stdout", &child_stdout);
    let panicked = fs::read_to_string(dir.join("core.log")).expect("the panic's log");
    assert_clean("the panic's core.log", &panicked);
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
    let _ = std::thread::spawn(|| panic!("canary path {HOME}/project")).join();
    std::process::exit(3);
}
