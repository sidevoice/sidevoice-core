//! `GET /api/logs` on the real binary: who may read the core's own log, that reading it writes nothing, that it goes on
//! across the rotation the real writer performs, and that the connector link has no such method (the core asks the connector for its log; the connector cannot ask the core for the core's).

mod support;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::Duration;

use serde_json::{json, Value};
use support::*;

const LOG_BYTES: usize = 5_000_000;

fn texts(page: &Value) -> Vec<String> {
    page["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line["text"].as_str().expect("text").to_owned())
        .collect()
}

async fn page(core: &Core, token: &str, query: &str) -> Value {
    let reply = core
        .get(&format!("/api/logs?{query}"))
        .token(token)
        .send()
        .await;
    assert_eq!(reply.status, 200, "{query}: {:?}", reply.body);
    reply.json()
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_paired_device_reads_the_log_on_either_listener() {
    let root = support::private_tempdir();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Viewer").await;
    for (listener, reply) in [
        ("TCP", core.get("/api/logs").send().await),
        ("socket", core.local("GET", "/api/logs").send().await),
        (
            "TCP with another token",
            core.get("/api/logs").token("not-a-token").send().await,
        ),
        (
            "socket with another token",
            core.local("GET", "/api/logs")
                .token("not-a-token")
                .send()
                .await,
        ),
    ] {
        assert_eq!(reply.status, 401, "{listener}: {reply:?}");
        assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
        assert!(reply.json().get("lines").is_none(), "{listener}");
    }
    for reply in [
        core.get("/api/logs").token(&token).send().await,
        core.local("GET", "/api/logs").token(&token).send().await,
    ] {
        assert_eq!(reply.status, 200, "{reply:?}");
        assert!(reply.json()["lines"].is_array());
    }
    assert_eq!(
        core.post("/api/logs", json!({}))
            .token(&token)
            .send()
            .await
            .status,
        405
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_connector_link_has_no_log_method() {
    let root = support::private_tempdir();
    let core = Launch::new(root.path().join("core")).start();
    let mut rpc = Rpc::hello(&core).await;
    for method in ["logs.read", "logs.tail", "logs.subscribe"] {
        let answer = rpc.request(method, json!({"limit": 1})).await;
        assert_eq!(answer["error"]["code"], -32601, "{method}: {answer}");
        assert!(answer.get("result").is_none(), "{method}: {answer}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_writes_no_log_line_at_any_level() {
    let root = support::private_tempdir();
    let log = root.path().join("core.log");
    let core = Launch::new(root.path().join("core"))
        .env("SIDEVOICE_LOG", "trace")
        .start();
    let token = core.pair_local("Viewer").await;
    let ready = eventually(STEP, "the ready line in the log", || async {
        let tail = page(&core, &token, "limit=1000").await;
        texts(&tail)
            .iter()
            .any(|line| line.contains("accepting connections"))
            .then_some(tail)
    })
    .await;
    // Everything above is the setup's: from here on only reads happen.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = fs::read(&log).unwrap();
    let cursor = ready["cursor"].as_str().unwrap().to_owned();
    let mut waits = Vec::new();
    for _ in 0..3 {
        waits.push(page(&core, &token, &format!("after={cursor}&limit=5&wait=1")).await);
    }
    assert!(waits.iter().all(|wait| texts(wait).is_empty()));
    page(&core, &token, "limit=3").await;
    page(&core, &token, "after=start&limit=2").await;
    for (query, status) in [("limit=0", 400), ("after=0123456789abcdef.0", 410)] {
        let reply = core
            .get(&format!("/api/logs?{query}"))
            .token(&token)
            .send()
            .await;
        assert_eq!(reply.status, status, "{query}");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fs::read(&log).unwrap(), before, "a read wrote to core.log");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_goes_on_across_the_rotation_the_writer_does_without_loss_or_repeat() {
    let root = support::private_tempdir();
    let log = root.path().join("core.log");
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Viewer").await;
    let started = page(&core, &token, "limit=1000").await;
    let cursor = started["cursor"].as_str().unwrap().to_owned();
    // The log reaches its limit, as other writers' lines would take it there: the core's next line rotates it.
    let filler: String = (0..LOG_BYTES / 1000)
        .map(|n| format!("filler {n:05} {}\n", "f".repeat(1000 - 14)))
        .collect();
    assert!(filler.len() >= LOG_BYTES);
    OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(filler.as_bytes())
        .unwrap();
    // Pairing the machine again replaces its device: the first token is gone, the cursor is not tied to it.
    let token = core.pair_local("Second viewer").await;
    assert!(
        log.with_extension("log.1").exists(),
        "the core's own writer rotated the log"
    );
    let mut seen = Vec::new();
    let mut after = cursor;
    loop {
        let next = page(&core, &token, &format!("after={after}&limit=1000")).await;
        seen.extend(texts(&next));
        after = next["cursor"].as_str().unwrap().to_owned();
        if next["more"] != true {
            break;
        }
    }
    let fillers: Vec<&String> = seen
        .iter()
        .filter(|line| line.starts_with("filler"))
        .collect();
    assert_eq!(fillers.len(), LOG_BYTES / 1000);
    for (n, line) in fillers.iter().enumerate() {
        assert!(line.starts_with(&format!("filler {n:05} ")), "{n}: {line}");
    }
    let paired = seen
        .iter()
        .filter(|line| line.contains("this machine's own device paired"))
        .count();
    assert_eq!(paired, 1, "the line written after the rotation, once");
    assert!(
        seen.last().unwrap().contains("paired"),
        "in order: the new generation comes last"
    );
    // Nothing is left: the cursor is at the end.
    assert!(texts(&page(&core, &token, &format!("after={after}")).await).is_empty());
}
