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

fn seeds(home: &str, runtime: &[String]) -> Vec<String> {
    let mut seeds: Vec<String> = [home, SECRET, WORDS, URL, "canary-pass", "canary-q", THREAD]
        .map(str::to_owned)
        .into();
    // Values this run minted: the pairing code, the device and the resume tokens.
    seeds.extend(runtime.iter().cloned());
    // On Windows the seeded home is the real one, so its name is not ours to seed.
    if !cfg!(windows) {
        seeds.push(SEED_HOME.to_owned());
    }
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

fn assert_clean(what: &str, text: &str, home: &str, runtime: &[String]) {
    for seed in seeds(home, runtime) {
        assert!(!text.contains(&seed), "{what} holds {seed:?}:\n{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_secret_or_said_reaches_the_core_log() {
    if let Ok(dir) = std::env::var(PANIC_DIR) {
        return panic_child(&dir);
    }
    // The data directory, and so core.log, sit under the seeded home. `dirs` reads HOME on Unix but asks the system on
    // Windows, so there the seeded home is the real one and the data directory stays in the temporary one.
    let root = support::private_tempdir();
    let home = if cfg!(windows) {
        dirs::home_dir().unwrap()
    } else {
        root.path().join(SEED_HOME)
    };
    let data = if cfg!(windows) { root.path() } else { &home }.join(".sidevoice");
    if !home.exists() {
        support::private_dir(&home);
    }
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
    // A pairing secret that was never issued is refused, and the one that was is spent: neither is logged.
    let wrong = core
        .post("/api/device/pair", json!({"secret": SECRET, "name": WORDS}))
        .send()
        .await;
    assert_eq!(wrong.status, 403, "{wrong:?}");

    // A call, a conversation named by the agent, a turn id that is a sentence, and a turn that is said.
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let (mut peer, binding) = connector_with_binding(&core, "canary", THREAD).await;
    let issued = peer.call("device.pairing_code", json!({})).await;
    let code = issued["payload"]["secret"].as_str().unwrap().to_owned();
    let redeemed = core
        .post("/api/device/pair", json!({"secret": code, "name": WORDS}))
        .send()
        .await;
    assert_eq!(redeemed.status, 200, "{redeemed:?}");
    let minted = vec![
        code,
        issued["code"].as_str().unwrap().to_owned(),
        redeemed.json()["token"].as_str().unwrap().to_owned(),
        token.clone(),
        core.connector_token(),
        browser.welcome["resume"]["token"]
            .as_str()
            .unwrap()
            .to_owned(),
    ];
    let focus = core.select(&token, &session, THREAD).await;
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
    // Typed words, to the same conversation.
    core.text(&token, (&session, THREAD, &focus), WORDS, &message_id())
        .await;
    accept_delivery(&mut peer).await;
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
    assert_clean("core.log", &log, &home, &minted);
    assert_clean("stderr", &stderr, &home, &minted);
    // Positive controls: the flow was logged, and a hostile id became `invalid`.
    for expected in [
        "key=\"runtime.log_start\"",
        "service.name=\"sidevoice-core\"",
        "os.type=",
        "host.arch=",
        "key=\"runtime.log_ready\"",
        "call{session=",
        "connector_link{connector=",
        "binding{connector=",
        "input{kind=\"text\"",
        "speech{binding=",
        "turn=invalid",
        "turn=turn-ok-1",
        "result=\"paired_local\"",
        "result=\"paired\"",
        "error.type=\"core.pairing-refused\"",
        "expires_in_s=",
        "error.type=\"core.rate-limited\"",
        "error.type=\"core.connection-limit\"",
        "key=\"runtime.log_stop\"",
    ] {
        assert!(log.contains(expected), "no {expected:?} in:\n{log}");
    }
    // The v1 names are gone.
    for old in [" code=", " http_status=", " exit_code=", " version="] {
        assert!(!log.contains(old), "{old:?} is a v1 field name:\n{log}");
    }
    assert!(
        log.contains(&format!("conversation={}", IdHash(THREAD))),
        "{log}"
    );
    assert!(
        log.contains("DEBUG") && log.contains("kind=invalid"),
        "the page's error report, by kind only:\n{log}"
    );

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
    assert_clean("the panicking process's stderr", &child_stderr, &home, &[]);
    assert_clean("the panicking process's stdout", &child_stdout, &home, &[]);
    let panicked = fs::read_to_string(dir.join("core.log")).expect("the panic's log");
    assert_clean("the panic's core.log", &panicked, &home, &[]);
    // Two panics in the child: the first with a formatted message (the path never printed), the second a literal.
    // Each is a `panic` event and, for the file only, a `crash_backtrace` event.
    let records = |target: &str| {
        panicked
            .lines()
            .filter(|l| l.contains(&format!("ERROR {target}:")))
            .count()
    };
    assert_eq!(
        (records("panic"), records("crash_backtrace")),
        (2, 2),
        "{panicked}"
    );
    assert!(
        panicked.contains("exception.message=\"<formatted panic message omitted>\"")
            && panicked.contains("exception.message=\"canary literal panic\"")
            && panicked.contains("exception.type=\"panic\"")
            && panicked.contains("thread.name=")
            && panicked.contains("service.version=")
            && panicked.contains("code.file.path=")
            && panicked.contains("code.line.number="),
        "{panicked}"
    );
    assert!(
        panicked
            .lines()
            .filter(|line| line.contains("ERROR panic:"))
            .all(|line| !line.contains("exception.stacktrace")),
        "the backtrace is its own event:\n{panicked}"
    );
    assert!(
        panicked
            .lines()
            .any(|line| line.contains("ERROR crash_backtrace:")
                && line.contains("exception.stacktrace=")),
        "{panicked}"
    );
    // Each panic also left a crash record on the machine: the closed fields, the literal or the marker, no canary.
    let stored: Vec<Value> = fs::read_dir(dir.join("core").join("crashes"))
        .expect("the crash records")
        .map(|entry| {
            let text = fs::read_to_string(entry.unwrap().path()).unwrap();
            assert_clean("a crash record", &text, &home, &[]);
            serde_json::from_str(&text).unwrap()
        })
        .collect();
    let mut messages: Vec<&str> = stored
        .iter()
        .map(|record| record["exception"]["message"].as_str().unwrap())
        .collect();
    messages.sort_unstable();
    assert_eq!(
        messages,
        ["<formatted panic message omitted>", "canary literal panic"]
    );
    for record in &stored {
        assert!(record["location"]["line"].as_u64().unwrap() > 0);
        assert!(!record["backtrace"].as_array().unwrap().is_empty());
        assert!(record["backtrace"][0]["addr"].as_u64().unwrap() > 0);
        assert!(
            !record["log_tail"].to_string().contains("ERROR panic:"),
            "the panic itself is the record, not its tail"
        );
    }

    // The default hook is not chained, and a log that was written needs no stderr line: stderr holds neither.
    assert!(
        !child_stderr.contains("panicked at") && !child_stderr.contains("stack backtrace"),
        "{child_stderr}"
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
    let _ = std::thread::spawn(|| panic!("canary literal panic")).join();
    std::process::exit(3);
}
