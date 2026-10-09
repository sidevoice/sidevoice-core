//! The call socket carries text and events only: the call's voice module reports the person's turns
//! (`voice-user-turn`) and what became of each reply (`voice-playback`); the room sends replies as text
//! (`voice-reply`). Every client message carries a `client_msg_id` and is acknowledged.

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use support::*;

const THREAD: &str = "frames-thread";

fn row<'a>(history: &'a [Value], id: &str) -> &'a Value {
    history
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("no row {id} in {history:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spoken_turn_reaches_the_conversation_and_a_cancelled_one_does_not() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (mut peer, binding) = v2_with_binding(&core, "frames", THREAD).await;
    let mut browser = core.join(&token, json!({"ui_language": "en"})).await;
    assert!(
        browser.welcome.get("sample_rate").is_none(),
        "no audio format"
    );
    core.select(&token, &browser.session, THREAD).await;

    let revision = browser.say("Run the tests, please.").await;
    let delivery = accept_delivery(&mut peer).await;
    assert_eq!(delivery.data["binding_id"], binding);
    assert_eq!(delivery.data["text"], "Run the tests, please.");
    assert_eq!(delivery.data["revision"], revision);

    // A turn that ended with nothing said, or that the person cancelled, sends nothing.
    browser
        .report("voice-user-turn", json!({"phase": "started"}))
        .await;
    let cancelled = browser
        .wait("voice-user-turn", STEP, |turn| turn["phase"] == "started")
        .await["revision"]
        .clone();
    browser
        .report(
            "voice-user-turn",
            json!({"phase": "cancelled", "revision": cancelled}),
        )
        .await;
    // Its words, arriving late, are refused: the turn ended.
    let late = browser
        .report(
            "voice-user-turn",
            json!({"phase": "finished", "revision": cancelled, "text": "Never mind."}),
        )
        .await;
    let refusal = browser.frame("error").await;
    assert_eq!(
        (refusal["key"].as_str(), refusal["client_msg_id"].as_str()),
        (Some("room.input_ended"), Some(late.as_str()))
    );
    let history = core.history(&token, THREAD).await;
    let said: Vec<_> = history
        .iter()
        .filter(|row| row["role"] == "user")
        .map(|row| row["text"].clone())
        .collect();
    assert_eq!(said, [json!("Run the tests, please.")]);
    browser.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replies_go_out_as_text_and_the_history_shows_how_far_each_was_heard() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (peer, binding) = v2_with_binding(&core, "frames", THREAD).await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    let revision = core.revision(&token, &session).await;

    let answer = publish(
        &peer,
        &binding,
        &session,
        revision,
        "cut",
        "A reply cut short.",
    )
    .await;
    assert_eq!(answer["status"], "queued", "{answer}");
    let reply = browser.frame("voice-reply").await;
    assert_eq!(
        reply,
        json!({"session_id": session, "utterance_id": "cut", "revision": revision, "reply_revision": revision,
            "thread_id": THREAD, "text": "A reply cut short.", "language": "en",
            "history_id": format!("{session}:voice:cut")})
    );
    browser.playback("cut", "playing").await;
    browser
        .report(
            "voice-playback",
            json!({"utterance_id": "cut", "status": "interrupted", "heard_chars": 7}),
        )
        .await;
    publish(
        &peer,
        &binding,
        &session,
        revision,
        "whole",
        "A reply heard whole.",
    )
    .await;
    browser.frame("voice-reply").await;
    browser.played("whole").await;

    let history = core.history(&token, THREAD).await;
    let cut = row(&history, &format!("{session}:voice:cut"));
    assert_eq!(
        (&cut["status"], &cut["audio_reason"], &cut["heard_chars"]),
        (&json!("interrupted"), &json!("user_interrupted"), &json!(7))
    );
    assert_eq!(
        row(&history, &format!("{session}:voice:whole"))["status"],
        "playback_finished"
    );

    // A report the room cannot read is refused with its key; one for a reply it never sent, too.
    let bad = browser
        .report(
            "voice-playback",
            json!({"utterance_id": "whole", "status": "paused"}),
        )
        .await;
    let refusal = browser.frame("error").await;
    assert_eq!(
        (refusal["key"].as_str(), refusal["client_msg_id"].as_str()),
        (Some("room.receipt_invalid"), Some(bad.as_str()))
    );
    // How far a reply was heard is a count of its characters, never past its end: anything else is refused
    // before it changes what the history says.
    for heard in [
        json!(-1),
        json!("7"),
        json!(1.5),
        json!(21),
        json!(u64::MAX),
    ] {
        let bad = browser
            .report(
                "voice-playback",
                json!({"utterance_id": "whole", "status": "interrupted", "heard_chars": heard}),
            )
            .await;
        let refusal = browser.frame("error").await;
        assert_eq!(
            (refusal["key"].as_str(), refusal["client_msg_id"].as_str()),
            (Some("room.receipt_invalid"), Some(bad.as_str())),
            "heard_chars {heard}"
        );
    }
    assert_eq!(
        row(
            &core.history(&token, THREAD).await,
            &format!("{session}:voice:whole")
        )["status"],
        "playback_finished"
    );
    browser.playback("never-sent", "playing").await;
    assert_eq!(browser.frame("error").await["key"], "room.stale_utterance");
    browser.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_message_without_an_id_is_refused_in_the_call_s_language() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, json!({"ui_language": "fr"})).await;
    // A language the core does not offer: refused with its key, and the call goes on.
    assert_eq!(
        browser.frame("error").await["key"],
        "settings.ui_language_invalid"
    );
    browser
        .send(
            "voice-settings",
            json!({"session_id": browser.session, "ui_language": "es"}),
        )
        .await;
    browser
        .send(
            "voice-user-turn",
            json!({"session_id": browser.session, "phase": "started"}),
        )
        .await;
    let refusal = browser.frame("error").await;
    assert_eq!(refusal["key"], "room.request_invalid");
    assert_eq!(refusal["message"], "La solicitud a la sala no es válida.");
    browser
        .none_of(&["voice-user-turn"], Duration::from_millis(300))
        .await;
    browser.close().await;
}
