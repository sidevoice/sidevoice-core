//! A call whose socket drops comes back as itself: the page reconnects with its session's token and the last
//! frame it handled, gets what it missed, and nothing it sends again is taken twice. Speech the person never
//! heard is not played late.

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use support::*;

const THREAD: &str = "resumed-thread";

fn own_rows(history: &[Value], session: &str, role: &str) -> Vec<Value> {
    history
        .iter()
        .filter(|row| row["session"] == session && row["role"] == role)
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_call_resumes_without_losing_or_repeating_anything() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (_peer, _binding) = v2_with_binding(&core, "resumed", THREAD).await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    assert_eq!(browser.welcome["resumed"], false);
    let resume = browser.welcome["resume"]["token"]
        .as_str()
        .unwrap()
        .to_owned();

    // The person is speaking when the network goes: the turn is open on the page, which transcribes it meanwhile.
    browser
        .send(
            "voice-user-turn",
            json!({"session_id": session, "client_msg_id": "turn-started", "turn_id": "across", "phase": "started"}),
        )
        .await;
    let started = browser
        .wait("voice-user-turn", STEP, |turn| turn["phase"] == "started")
        .await;
    assert_eq!(
        started["turn_id"], "across",
        "the room names the turn it started"
    );
    let last_seq = browser.last_seq;
    browser.drop_link().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(core.calls().await, 1, "the parked call keeps its seat");

    let mut back = core.resume(&token, &session, &resume, last_seq).await;
    assert_eq!(back.session, session, "the same call");
    assert_eq!(back.welcome["resumed"], true, "{}", back.welcome);
    assert_ne!(
        back.welcome["resume"]["token"],
        resume.as_str(),
        "a fresh token"
    );
    // The page sends the turn's words twice (its outbox retried): taken once, acknowledged both times.
    let finished = json!({"session_id": session, "client_msg_id": "turn-finished", "phase": "finished",
        "turn_id": "across", "text": "Said across the drop"});
    for _ in 0..2 {
        back.send("voice-user-turn", finished.clone()).await;
        assert_eq!(
            back.frame("voice-ack").await["client_msg_id"],
            "turn-finished"
        );
    }
    // A turn the page spoke and transcribed while it had no room, sent twice: one row.
    let offline = json!({"session_id": session, "client_msg_id": "offline-1", "phase": "finished",
        "turn_id": "away", "offline": true, "text": "Said while away"});
    back.send("voice-user-turn", offline.clone()).await;
    assert_eq!(back.frame("voice-ack").await["client_msg_id"], "offline-1");
    // Once taken, the offline turn's boundary is said as a started turn's is: after the one across the drop.
    let away = back
        .wait("voice-user-turn", STEP, |turn| turn["turn_id"] == "away")
        .await;
    assert_eq!(away["phase"], "started");
    assert!(away["revision"].as_u64() > started["revision"].as_u64());
    back.send("voice-user-turn", offline.clone()).await;
    assert_eq!(back.frame("voice-ack").await["client_msg_id"], "offline-1");
    let rows = own_rows(&core.history(&token, THREAD).await, &session, "user");
    let texts: Vec<_> = rows.iter().map(|row| row["text"].clone()).collect();
    assert_eq!(
        texts,
        [json!("Said across the drop"), json!("Said while away")]
    );
    assert_eq!(rows[1]["offline"], "offline");

    // The token was spent by the resume: the same one again is a new call.
    let stale = core.resume(&token, &session, &resume, 0).await;
    assert_ne!(stale.session, session);
    assert_eq!(stale.welcome["resumed"], false);
    assert_eq!(stale.welcome["resume_refused"], "unknown");
    stale.close().await;
    back.close().await;
    core.calls_become(0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_published_while_the_page_was_away_is_marked_unheard_not_played() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (peer, binding) = v2_with_binding(&core, "resumed", THREAD).await;
    let browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    let revision = core.revision(&token, &session).await;
    let resume = browser.welcome["resume"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let last_seq = browser.last_seq;
    browser.drop_link().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let answer = publish(
        &peer,
        &binding,
        &session,
        revision,
        "away",
        "Said while nobody listened",
    )
    .await;
    assert_eq!(answer["status"], "queued", "{answer}");

    let mut back = core.resume(&token, &session, &resume, last_seq).await;
    assert_eq!(back.welcome["resumed"], true);
    back.none_of(&["voice-reply"], Duration::from_millis(1500))
        .await;
    let rows = own_rows(&core.history(&token, THREAD).await, &session, "assistant");
    let row = rows.last().expect("the reply stays written");
    assert_eq!(
        (row["status"].as_str(), row["audio_reason"].as_str()),
        (Some("interrupted"), Some("unheard")),
        "{row}"
    );
    back.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_that_never_comes_back_gives_its_seat_back_when_the_park_ends() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core"))
        .env("VOICE_RESUME_SECONDS", "1")
        .env("VOICE_BROWSER_HEARTBEAT_SECONDS", "0.5")
        .env("VOICE_BROWSER_HEARTBEAT_MISSES", "2")
        .start();
    let token = core.pair_local("Browser").await;
    // Dropped without a word: parked, then released.
    let browser = core.join(&token, Value::Null).await;
    core.calls_become(1).await;
    browser.drop_link().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(core.calls().await, 1, "parked");
    core.calls_become(0).await;
    // Silent behind a proxy that never closes the socket: the heartbeat parks it, and the park ends it.
    let quiet = core.join(&token, Value::Null).await;
    core.calls_become(1).await;
    core.calls_become(0).await;
    drop(quiet);
    // A hang-up ends the call at once.
    let leaving = core.join(&token, Value::Null).await;
    core.calls_become(1).await;
    leaving.close().await;
    core.calls_become(0).await;
}
