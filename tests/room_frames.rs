//! The call socket carries text and events only: the call's voice module reports the person's turns
//! (`voice-user-turn`) and what became of each reply (`voice-playback`); the room sends replies as text
//! (`voice-reply`). Every client message carries a `client_msg_id` and is acknowledged.

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use support::*;
use tokio_tungstenite::tungstenite::Message;

const THREAD: &str = "frames-thread";

fn row<'a>(history: &'a [Value], id: &str) -> &'a Value {
    history
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("no row {id} in {history:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn delayed_socket_start_after_http_focus_change_keeps_captured_conversation() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (_peer_a, _) = connector_with_binding(&core, "captured-a", "captured-a").await;
    let (_peer_b, _) = connector_with_binding(&core, "captured-b", "captured-b").await;
    let mut browser = core.join(&token, Value::Null).await;
    let a_focus = core.select(&token, &browser.session, "captured-a").await;
    core.select(&token, &browser.session, "captured-b").await;
    browser
        .report(
            "voice-user-turn",
            json!({"phase":"started","turn_id":"a-turn",
        "thread_id":"captured-a","binding_id":a_focus}),
        )
        .await;
    let started = browser.turn("started", STEP).await;
    assert_eq!(started["thread_id"], "captured-a");
    assert_eq!(started["epoch"], 1);
    browser
        .report(
            "voice-user-turn",
            json!({"phase":"finished","turn_id":"a-turn",
        "text":"A's delayed words","thread_id":"captured-a","binding_id":a_focus}),
        )
        .await;
    let history = core.history(&token, "captured-a").await;
    let spoken = history
        .iter()
        .find(|r| r["text"] == "A's delayed words")
        .unwrap();
    let id = spoken["id"].as_str().unwrap();
    let resolved = core
        .get(&format!(
            "/api/presentation/history?thread_id=captured-a&resolve_id={id}&resolve_id=gone"
        ))
        .token(&token)
        .send()
        .await;
    assert_eq!(resolved.status, 200);
    assert_eq!(resolved.json()["resolved"][0]["id"], id);
    assert_eq!(resolved.json()["missing_ids"], json!(["gone"]));
    assert!(!core
        .history(&token, "captured-b")
        .await
        .iter()
        .any(|r| r["role"] == "user"));
    assert_eq!(core.participant(&token, "captured-a").await["epoch"], 1);
    assert_eq!(core.participant(&token, "captured-b").await["epoch"], 0);
    browser.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spoken_turn_reaches_the_conversation_and_a_cancelled_one_does_not() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (mut peer, binding) = connector_with_binding(&core, "frames", THREAD).await;
    let mut browser = core.join(&token, json!({"ui_language": "en"})).await;
    assert!(
        browser.welcome.get("sample_rate").is_none(),
        "no audio format"
    );
    let focus_binding = core.select(&token, &browser.session, THREAD).await;

    let revision = browser.say("Run the tests, please.").await;
    let delivery = accept_delivery(&mut peer).await;
    assert_eq!(delivery["binding_id"], binding);
    assert_eq!(delivery["text"], "Run the tests, please.");
    assert_eq!(delivery["revision"], revision);

    // A turn that ended with nothing said, or that the person cancelled, sends nothing.
    browser.start_turn("cancelled").await;
    browser
        .report(
            "voice-user-turn",
            json!({"phase": "cancelled", "turn_id": "cancelled"}),
        )
        .await;
    // Its words, arriving late, are refused: the turn ended.
    let late = browser
        .report(
            "voice-user-turn",
            json!({"phase": "finished", "turn_id": "cancelled", "text": "Never mind."}),
        )
        .await;
    let refusal = browser.frame("error").await;
    assert_eq!(
        (refusal["key"].as_str(), refusal["client_msg_id"].as_str()),
        (Some("room.input_ended"), Some(late.as_str()))
    );
    // A new reliable frame ID cannot revive the same cancelled turn as offline speech.
    let offline = browser
        .report(
            "voice-user-turn",
            json!({"phase":"finished","turn_id":"cancelled","offline":true,
                "thread_id":THREAD,"binding_id":focus_binding,"text":"Resurrected words"}),
        )
        .await;
    let refusal = browser.frame("error").await;
    assert_eq!(refusal["key"], "room.input_ended");
    assert_eq!(refusal["client_msg_id"], offline);
    let history = core.history(&token, THREAD).await;
    let said: Vec<_> = history
        .iter()
        .filter(|row| row["role"] == "user")
        .map(|row| row["text"].clone())
        .collect();
    assert_eq!(said, [json!("Run the tests, please.")]);
    browser.close().await;
}

/// The call names its turns (`turn_id`) and sends no revision: turns may overlap, each ends by its name, and a reply
/// written before a turn started answers an older turn, finished or not.
#[tokio::test(flavor = "multi_thread")]
async fn turns_are_named_by_the_call_and_may_overlap() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (mut peer, binding) = connector_with_binding(&core, "overlap", THREAD).await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    let focus = core.select(&token, &session, THREAD).await;

    let first = browser.start_turn("first").await;
    let second = browser.start_turn("second").await;
    assert!(second > first, "each turn its own boundary");
    // A duplicate start returns the same boundary and cannot advance the epoch.
    browser
        .report(
            "voice-user-turn",
            json!({"phase": "started", "turn_id": "first", "thread_id": THREAD, "binding_id": focus}),
        )
        .await;
    let duplicate = browser.frame("voice-user-turn").await;
    assert_eq!(duplicate["revision"], first);
    assert_eq!(duplicate["epoch"], 1);

    // The first turn's words come after the second turn started: both reach the conversation, each with its turn.
    browser
        .report(
            "voice-user-turn",
            json!({"phase": "finished", "turn_id": "first", "text": "First words"}),
        )
        .await;
    assert_eq!(browser.receipt("pending").await["turn_id"], "first");
    let delivered = accept_delivery(&mut peer).await;
    assert_eq!(
        (&delivered["text"], &delivered["revision"]),
        (&json!("First words"), &json!(first))
    );
    browser
        .report(
            "voice-user-turn",
            json!({"phase": "finished", "turn_id": "second", "text": "Second words"}),
        )
        .await;
    let delivered = accept_delivery(&mut peer).await;
    assert_eq!(delivered["revision"], second);
    // The agent answers the first turn after the second one's words: it is not spoken.
    let late = publish(
        &mut peer,
        &binding,
        &session,
        first,
        1,
        "late",
        "An old answer.",
    )
    .await;
    assert_eq!(
        (late["status"].as_str(), late["reason"].as_str()),
        (Some("text_only"), Some("newer_turn")),
        "{late}"
    );
    let history = core.history(&token, THREAD).await;
    let first_row = row(&history, &format!("{session}:user-turn:first"));
    assert_eq!(first_row["text"], "First words");

    // A turn the room does not hold cannot end.
    let unknown = browser
        .report(
            "voice-user-turn",
            json!({"phase": "finished", "turn_id": "never-started", "text": "Hm."}),
        )
        .await;
    let refusal = browser.frame("error").await;
    assert_eq!(
        (refusal["key"].as_str(), refusal["client_msg_id"].as_str()),
        (Some("room.input_ended"), Some(unknown.as_str()))
    );
    browser.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replies_go_out_as_text_and_the_history_shows_how_far_each_was_heard() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (mut peer, binding) = connector_with_binding(&core, "frames", THREAD).await;
    let mut browser = core.join(&token, Value::Null).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    let revision = core.revision(&token, &session).await;

    let answer = publish(
        &mut peer,
        &binding,
        &session,
        revision,
        0,
        "cut",
        "A reply cut short.",
    )
    .await;
    assert_eq!(answer["status"], "queued", "{answer}");
    let reply = browser.frame("voice-reply").await;
    assert_eq!(
        reply,
        json!({"session_id": session, "utterance_id": "cut", "revision": revision, "reply_revision": revision,"epoch":0,
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
        &mut peer,
        &binding,
        &session,
        revision,
        0,
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
        .report("voice-playback", json!({"utterance_id": "whole"}))
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
    let mut browser = core.join(&token, json!({"ui_language": 42})).await;
    // A language that is not text: refused with its key, and the call goes on.
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

/// The protocol only grows (#112): a newer page may name a turn phase, a playback status or a reason this core does
/// not know. Each report is acknowledged and refused by nobody; the unknown part changes nothing, the known part
/// counts.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_with_values_the_core_does_not_know_is_taken_and_its_known_part_counts() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let (mut peer, binding) = connector_with_binding(&core, "unknown", THREAD).await;
    let mut browser = core.join(&token, Value::Null).await;
    browser.no_errors = true;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;

    browser
        .report(
            "voice-user-turn",
            json!({"phase": "paused", "turn_id": "future"}),
        )
        .await;
    browser
        .none_of(&["error", "voice-user-turn"], Duration::from_millis(300))
        .await;

    let revision = core.revision(&token, &session).await;
    publish(
        &mut peer,
        &binding,
        &session,
        revision,
        0,
        "cut",
        "A reply cut short.",
    )
    .await;
    browser.frame("voice-reply").await;
    browser
        .report(
            "voice-playback",
            json!({"utterance_id": "cut", "status": "buffering"}),
        )
        .await;
    browser.playback("cut", "playing").await;
    browser
        .report(
            "voice-playback",
            json!({"utterance_id": "cut", "status": "interrupted", "reason": "phone_rang", "heard_chars": 7}),
        )
        .await;
    browser
        .none_of(&["error"], Duration::from_millis(300))
        .await;
    let history = core.history(&token, THREAD).await;
    let cut = row(&history, &format!("{session}:voice:cut"));
    assert_eq!(
        (&cut["status"], &cut["audio_reason"], &cut["heard_chars"]),
        (&json!("interrupted"), &json!("user_interrupted"), &json!(7))
    );
    browser.close().await;
}

/// An interface language this core does not offer (a newer page may offer more) is no error: a hello goes on in the
/// machine's language, and a settings change keeps the call's current one.
#[tokio::test(flavor = "multi_thread")]
async fn an_interface_language_the_core_does_not_offer_falls_back_without_an_error() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let token = core.pair_local("Browser").await;
    let mut browser = core.join(&token, json!({"ui_language": "fr"})).await;
    browser.no_errors = true;
    browser
        .none_of(&["error"], Duration::from_millis(300))
        .await;
    for language in ["es", "fr"] {
        browser
            .send(
                "voice-settings",
                json!({"session_id": browser.session, "ui_language": language}),
            )
            .await;
    }
    browser
        .none_of(&["error"], Duration::from_millis(300))
        .await;
    // Still Spanish: a refusal comes in the language the call kept.
    browser.no_errors = false;
    browser
        .send(
            "voice-user-turn",
            json!({"session_id": browser.session, "phase": "started"}),
        )
        .await;
    let refusal = browser.frame("error").await;
    assert_eq!(refusal["key"], "room.request_invalid");
    assert_eq!(refusal["message"], "La solicitud a la sala no es válida.");
    browser.close().await;
}

/// Every message a page sends, with fields this core does not know at the top level, nested, next to the frame's
/// `type` and `data`, and in query strings, and a frame type it does not know: all are taken as their known part says,
/// and no error frame is sent at any step.
#[tokio::test(flavor = "multi_thread")]
async fn every_page_message_with_fields_the_core_does_not_know_is_taken() {
    let root = tempfile::tempdir().unwrap();
    let core = Launch::new(root.path().join("core")).start();
    let paired = core
        .local("POST", "/api/device/local/pair")
        .json(json!({"name": "Browser", "added_later": {"nested": true}}))
        .send()
        .await;
    assert_eq!(paired.status, 200, "{paired:?}");
    let token = paired.json()["token"].as_str().unwrap().to_owned();
    let (mut peer, binding) = connector_with_binding(&core, "sweep", THREAD).await;
    let extra = json!({"added_later": {"nested": [1, {"deeper": true}]}});
    let mut browser = core
        .join(
            &token,
            json!({"ui_language": "en", "future": extra, "telemetry": {"future": extra}}),
        )
        .await;
    browser.no_errors = true;
    let session = browser.session.clone();
    browser
        .send(
            "voice-future-frame",
            json!({"session_id": session, "future": extra}),
        )
        .await;
    browser
        .send(
            "voice-settings",
            json!({"session_id": session, "ui_language": "en", "future": extra}),
        )
        .await;

    let selected = core
        .post(
            "/api/presentation/select",
            json!({"session_id": session, "thread_id": THREAD, "future": extra}),
        )
        .token(&token)
        .send()
        .await;
    assert_eq!(selected.status, 200, "{selected:?}");
    let focus = selected.json()["binding"]["binding_id"]
        .as_str()
        .expect("a focus")
        .to_owned();

    let typed = core
        .post(
            "/api/presentation/text",
            json!({"text": "Typed", "session_id": session, "thread_id": THREAD, "binding_id": focus,
                "message_id": message_id(), "future": extra}),
        )
        .token(&token)
        .send()
        .await;
    assert_eq!(typed.status, 200, "{typed:?}");
    assert_eq!(accept_delivery(&mut peer).await["text"], "Typed");

    // An extra key next to `type` and `data`.
    browser.send_raw(Message::text(
        json!({"type": "voice-user-turn", "added_later": extra, "data": {"session_id": session,
            "client_msg_id": "envelope", "phase": "started", "turn_id": "spoken",
            "thread_id": THREAD, "binding_id": focus, "future": extra}})
        .to_string(),
    ));
    browser
        .wait("voice-ack", STEP, |ack| ack["client_msg_id"] == "envelope")
        .await;
    browser.turn("started", STEP).await;
    browser
        .report(
            "voice-user-turn",
            json!({"phase": "finished", "turn_id": "spoken", "text": "Spoken",
                "timings_ms": {"future_ms": 3, "nested": extra}, "future": extra}),
        )
        .await;
    assert_eq!(accept_delivery(&mut peer).await["text"], "Spoken");

    let revision = core.revision(&token, &session).await;
    publish(
        &mut peer,
        &binding,
        &session,
        revision,
        revision.saturating_sub(1),
        "reply",
        "A reply.",
    )
    .await;
    browser.frame("voice-reply").await;
    for status in ["playing", "heard"] {
        browser
            .report(
                "voice-playback",
                json!({"utterance_id": "reply", "status": status, "timings_ms": extra, "future": extra}),
            )
            .await;
    }
    let reported = core
        .post(
            "/api/presentation/client-error",
            json!({"session_id": session, "kind": "future", "message": "m", "future": extra}),
        )
        .token(&token)
        .send()
        .await;
    assert_eq!(reported.json()["status"], "recorded", "{reported:?}");

    // Query parameters the core does not read.
    let snapshot = core
        .get(&format!(
            "/api/presentation?session_id={session}&added_later=1"
        ))
        .token(&token)
        .send()
        .await;
    assert_eq!(snapshot.json()["room"]["revision"], revision);
    let history = core
        .get(&format!(
            "/api/presentation/history?thread_id={THREAD}&added_later=1"
        ))
        .token(&token)
        .send()
        .await;
    assert_eq!(
        row(
            history.json()["messages"].as_array().unwrap(),
            &format!("{session}:voice:reply")
        )["status"],
        "playback_finished"
    );
    let participants = core
        .get("/api/presentation/participants?added_later=1")
        .token(&token)
        .send()
        .await;
    assert!(
        participants.json()["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["thread_id"] == THREAD),
        "{participants:?}"
    );
    let latency = core
        .get(&format!(
            "/api/presentation/latency?session_id={session}&added_later=1"
        ))
        .token(&token)
        .send()
        .await;
    assert_eq!(latency.status, 200, "{latency:?}");
    browser
        .none_of(&["error"], Duration::from_millis(300))
        .await;

    // A resume with fields the core does not know lands on the same call.
    let resume = browser.welcome["resume"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let last_seq = browser.last_seq;
    browser.drop_link().await;
    let mut back = Browser::new(core.open_call(&token).await);
    back.send(
        "voice-hello",
        json!({"added_later": extra, "resume": {"session_id": session, "token": resume,
            "last_seq": last_seq, "added_later": extra}}),
    )
    .await;
    let welcome = back.frame("voice-session").await;
    assert_eq!(
        (&welcome["session_id"], &welcome["resumed"]),
        (&json!(session), &json!(true)),
        "{welcome}"
    );
    back.close().await;
}
