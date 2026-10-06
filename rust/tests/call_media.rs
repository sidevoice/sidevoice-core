//! A spoken call end to end: a recorded voice into the call socket (or a WebRTC track), the real voice detectors
//! deciding the turn, the browser transcribing it, the conversation answering, and the reply played on the device
//! or rendered once by a cloud voice and replayed from what was rendered. The cloud voice and the telemetry
//! collector are local fakes the core is pointed at through the `hosted-fixtures` overrides.
//!
//! Needs the detector models staged in `RUSTVANI_CACHE_DIR` (`cargo xtask models`).

mod support;

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use support::webrtc_peer::RtcBrowser;
use support::*;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const THREAD: &str = "spoken-thread";
const OTHER: &str = "other-thread";

fn timer(merge_window: f64) -> Value {
    json!({"turn_end_mode": "timer", "user_speech_timeout": 0.5, "merge_window_secs": merge_window})
}

fn decode(audio: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(audio.as_str().expect("base64 audio"))
        .expect("valid base64")
}

/// A core with a paired browser and a connector holding one conversation, plus a second one on `OTHER`.
struct Call {
    _root: tempfile::TempDir,
    core: Core,
    token: String,
    peer: Sio,
    binding: String,
}

async fn call(configure: impl FnOnce(&Path, Launch) -> Launch) -> Call {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    let core = configure(root.path(), Launch::new(&data)).start();
    let token = core.pair_local("Browser").await;
    let (peer, binding) = v2_with_binding(&core, "spoken", THREAD).await;
    Call {
        _root: root,
        core,
        token,
        peer,
        binding,
    }
}

/// Speaks `pcm` and the silence that ends a turn, transcribes what the core asks for as `text`; the finished turn
/// and its pending receipt.
async fn spoken_turn(browser: &mut Browser, pcm: &[u8], text: &str) -> (Value, Value) {
    browser.speak(pcm).await;
    browser.speak(&silence(4.0)).await;
    transcribed_turn(browser, text).await
}

/// Answers the core's next transcription request as `text`; the finished turn and its pending receipt.
async fn transcribed_turn(browser: &mut Browser, text: &str) -> (Value, Value) {
    let ask = browser
        .frame_within("voice-transcribe", Duration::from_secs(40))
        .await;
    assert_eq!(ask["session_id"], browser.session.as_str());
    let recorded = decode(&ask["audio_base64"]);
    assert!(
        recorded.starts_with(b"RIFF") && recorded.len() > 10_000,
        "a WAV of the turn"
    );
    browser.transcript(&ask, text).await;
    let finished = browser.turn("finished", Duration::from_secs(20)).await;
    assert_eq!(finished["text"], text, "{finished}");
    let receipt = browser.receipt("pending").await;
    assert_eq!(receipt["session_id"], browser.session.as_str());
    (finished, receipt)
}

async fn synthesized(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path().contains("/v1/text-to-speech/"))
        .map(|request| serde_json::from_slice(&request.body).unwrap_or(Value::Null))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spoken_turn_reaches_the_conversation_and_its_reply_plays_on_the_device() {
    let _serial = serial().await;
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/metrics"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;
    let mut call =
        call(|_, launch| launch.env("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri())).await;
    let (core, token) = (&call.core, call.token.clone());
    let pcm = speech();
    for mode in ["smart_turn", "timer"] {
        let mut browser = core
            .join(
                &token,
                json!({"turn_end_mode": mode, "merge_window_secs": 0, "user_speech_timeout": 0.5,
                    "smart_turn_min_silence": 0.5, "smart_turn_max_silence": 1.0}),
            )
            .await;
        let session = browser.session.clone();
        core.select(&token, &session, THREAD).await;
        browser.speak(&silence(1.0)).await;
        browser
            .none_of(
                &["voice-transcribe", "voice-user-turn", "voice-input-receipt"],
                Duration::from_millis(800),
            )
            .await;
        let (finished, receipt) =
            spoken_turn(&mut browser, &pcm, "Hola from the recorded call").await;
        assert_eq!(receipt["revision"], finished["revision"]);
        accept_delivery(&mut call.peer).await;
        browser.receipt("delivered").await;

        let revision = core.revision(&token, &session).await;
        let uid = format!("{mode}-{}", message_id());
        let answer = publish(
            &call.peer,
            &call.binding,
            &session,
            revision,
            &uid,
            "Reply from the conversation",
        )
        .await;
        assert_eq!(answer["status"], "queued");
        let speech = browser.frame("voice-speech").await;
        assert_eq!(
            (speech["utterance_id"].as_str(), speech["place"].as_str()),
            (Some(uid.as_str()), Some("device"))
        );
        core.played(&token, &session, &uid, speech["revision"].as_u64().unwrap())
            .await;

        // The latency of that reply, for the device that heard it and nobody else.
        let latency = format!("/api/presentation/latency?session_id={session}");
        let trace = core.get(&latency).token(&token).send().await;
        assert_eq!(trace.status, 200, "{trace:?}");
        let trace = trace.json();
        assert_eq!(trace["session_id"], session.as_str());
        let row = trace["replies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["utterance_id"] == uid.as_str())
            .expect("the reply's latency row")
            .clone();
        assert!(row["input_ms"]["audio_ms"].as_f64().unwrap() > 0.0, "{row}");
        assert!(
            row["server_ms"]["input_queued_to_reply_received_ms"]
                .as_f64()
                .unwrap()
                >= 0.0,
            "{row}"
        );
        assert_eq!(core.get(&latency).send().await.status, 401);
        let code = call.peer.call("device.pairing_code", json!({})).await.data;
        let other = core
            .post(
                "/api/device/pair",
                json!({"secret": code["payload"]["secret"], "name": "Another browser"}),
            )
            .send()
            .await
            .json()["token"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(core.get(&latency).token(&other).send().await.status, 404);

        if mode == "timer" {
            // Speaking over a reply interrupts it; the interrupted reply takes no more receipts.
            let stale = format!("barge-{}", message_id());
            publish(
                &call.peer,
                &call.binding,
                &session,
                revision,
                &stale,
                "A reply spoken over",
            )
            .await;
            let interrupted = browser.frame("voice-speech").await;
            browser.speak(&pcm).await;
            browser.speak(&silence(4.0)).await;
            browser.frame("voice-cancel").await;
            let ask = browser
                .frame_within("voice-transcribe", Duration::from_secs(30))
                .await;
            let late = core
                .receipt(
                    &token,
                    &session,
                    &stale,
                    interrupted["revision"].as_u64().unwrap(),
                    "playing",
                )
                .await;
            assert_eq!(late, 409);
            browser
                .transcript(&ask, "Follow-up after the interruption")
                .await;
            browser.turn("finished", Duration::from_secs(20)).await;
        }
    }
    eventually(Duration::from_secs(10), "the turn's telemetry", || async {
        let received = collector.received_requests().await.unwrap_or_default();
        received
            .iter()
            .any(|request| {
                String::from_utf8_lossy(&request.body).contains("sidevoice.turn.endpoint_silence")
            })
            .then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cloud_reply_is_rendered_once_and_every_replay_plays_what_was_rendered() {
    let _serial = serial().await;
    let voice = MockServer::start().await;
    let chunks = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/rust_t4/eleven_timestamp_chunks.json"),
    )
    .expect("the timestamp fixture");
    Mock::given(method("POST"))
        .and(path_regex(
            "^/v1/text-to-speech/fixturevoice/stream/with-timestamps",
        ))
        .and(header("xi-api-key", "fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(chunks, "application/json"))
        .mount(&voice)
        .await;
    let gate = std::env::temp_dir().join(format!("sidevoice-replay-gate-{}", message_id()));
    let entered = PathBuf::from(format!("{}.entered", gate.display()));
    let mut integrations = PathBuf::new();
    let mut call = call(|root, launch| {
        let data = root.join("core");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&data)
            .unwrap();
        integrations = data.join("integrations.json");
        std::fs::write(&integrations, r#"{"elevenlabs": "fixture-key"}"#).unwrap();
        std::fs::set_permissions(
            &integrations,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        launch
            .env("SIDEVOICE_ELEVENLABS_FIXTURE_BASE", voice.uri())
            .env(
                "SIDEVOICE_FIXTURE_REPLAY_RENDER_GATE",
                gate.display().to_string(),
            )
    })
    .await;
    let (core, token) = (&call.core, call.token.clone());
    let pcm = speech();
    let mut settings = timer(0.0);
    settings["tts"] = json!({"place": "elevenlabs", "model": "eleven_v3", "options": {"voice": {"en": "fixturevoice"}}});
    let mut browser = core.join(&token, settings).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    spoken_turn(&mut browser, &pcm, "Hola from the recorded call").await;
    accept_delivery(&mut call.peer).await;

    let revision = core.revision(&token, &session).await;
    let uid = format!("cloud-{}", message_id());
    publish(
        &call.peer,
        &call.binding,
        &session,
        revision,
        &uid,
        "Cloud fixture reply",
    )
    .await;
    let audio = browser
        .frame_within("voice-speech-audio", Duration::from_secs(20))
        .await;
    assert_eq!(
        (audio["utterance_id"].as_str(), audio["place"].as_str()),
        (Some(uid.as_str()), Some("elevenlabs"))
    );
    assert_eq!(decode(&audio["audio_base64"]), [1, 2, 3, 4]);
    assert_eq!(
        synthesized(&voice).await.last().unwrap()["text"],
        "Cloud fixture reply"
    );
    core.played(&token, &session, &uid, audio["revision"].as_u64().unwrap())
        .await;
    let history_id = audio["history_id"].as_str().unwrap().to_owned();
    let rows = core
        .get(&format!(
            "/api/presentation/history?thread_id={THREAD}&session_id={session}"
        ))
        .token(&token)
        .send()
        .await
        .json();
    let row = rows["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == history_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["replayable"], true, "{row}");
    let rendered = synthesized(&voice).await.len();

    let replay = || {
        core.post(
            "/api/presentation/replay",
            json!({"session_id": session, "history_id": history_id}),
        )
        .token(&token)
        .send()
    };
    let replayed = replay().await;
    assert_eq!(replayed.status, 200, "{replayed:?}");
    let replayed = replayed.json();
    assert_eq!(replayed["history_id"], history_id.as_str());
    assert_eq!(
        browser.frame("voice-replay").await["replies"][0]["utterance_id"],
        replayed["utterance_id"]
    );
    let again = browser
        .frame_within("voice-speech-audio", Duration::from_secs(20))
        .await;
    assert_eq!(again["utterance_id"], replayed["utterance_id"]);
    assert_eq!(again["audio_base64"], audio["audio_base64"]);
    assert_eq!(
        synthesized(&voice).await.len(),
        rendered,
        "a replay never pays for synthesis again"
    );
    core.played(
        &token,
        &session,
        replayed["utterance_id"].as_str().unwrap(),
        again["revision"].as_u64().unwrap(),
    )
    .await;

    // A burst of replays fills the room; a live reply still gets its turn after them.
    let mut burst = Vec::new();
    for _ in 0..16 {
        let answer = replay().await;
        assert_eq!(answer.status, 200, "{answer:?}");
        burst.push(answer.json()["utterance_id"].as_str().unwrap().to_owned());
    }
    let full = replay().await;
    let messages: Value = serde_json::from_slice(
        &std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("rust/messages/en.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(full.status, 429);
    assert_eq!(full.json()["detail"], messages["room.replay_full"]);
    let live = format!("live-{}", message_id());
    publish(
        &call.peer,
        &call.binding,
        &session,
        revision,
        &live,
        "Live after the replay burst",
    )
    .await;
    let mut expected = vec![burst[0].clone()];
    expected.extend(burst[1..].iter().rev().cloned());
    for utterance in expected {
        let played = browser
            .frame_within("voice-speech-audio", Duration::from_secs(20))
            .await;
        assert_eq!(played["utterance_id"], utterance.as_str());
        assert_eq!(played["audio_base64"], audio["audio_base64"]);
        core.played(
            &token,
            &session,
            &utterance,
            played["revision"].as_u64().unwrap(),
        )
        .await;
    }
    let live_audio = browser
        .frame_within("voice-speech-audio", Duration::from_secs(20))
        .await;
    assert_eq!(live_audio["utterance_id"], live.as_str());
    assert_eq!(
        synthesized(&voice).await.last().unwrap()["text"],
        "Live after the replay burst"
    );
    assert_eq!(
        synthesized(&voice).await.len(),
        rendered + 1,
        "replays never pay for synthesis"
    );
    core.played(
        &token,
        &session,
        &live,
        live_audio["revision"].as_u64().unwrap(),
    )
    .await;
    let snapshot = core
        .get(&format!("/api/presentation?session_id={session}"))
        .token(&token)
        .send()
        .await
        .json();
    assert!(snapshot["room"]["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["replay_of"].is_null()));
    let original = core.history(&token, THREAD).await;
    let original = original
        .iter()
        .find(|row| row["id"] == history_id.as_str())
        .unwrap();
    assert_eq!(original["status"], "playback_finished");

    // A replay held while the user starts talking plays after the turn, from its pinned audio, even with the
    // provider key gone.
    std::fs::write(&gate, "hold").unwrap();
    let held = replay().await;
    assert_eq!(held.status, 200, "{held:?}");
    let held = held.json()["utterance_id"].as_str().unwrap().to_owned();
    assert_eq!(
        browser.frame("voice-replay").await["replies"][0]["utterance_id"],
        held.as_str()
    );
    until(STEP, "the held replay to reach its render", || {
        std::fs::read_to_string(&entered).ok()
    });
    assert_eq!(std::fs::read_to_string(&entered).unwrap(), held);
    let microphone = browser.microphone();
    let talking = {
        let pcm = pcm.clone();
        tokio::spawn(async move { microphone.speak(&pcm).await })
    };
    browser.turn("started", STEP).await;
    std::fs::write(&integrations, "{}").unwrap();
    std::fs::remove_file(&gate).unwrap();
    talking.await.unwrap();
    browser.speak(&silence(4.0)).await;
    let ask = browser
        .frame_within("voice-transcribe", Duration::from_secs(30))
        .await;
    browser.transcript(&ask, "Resume the held replay").await;
    browser.turn("finished", STEP).await;
    let resumed = browser
        .frame_within("voice-speech-audio", Duration::from_secs(20))
        .await;
    assert_eq!(resumed["utterance_id"], held.as_str());
    assert_eq!(resumed["audio_base64"], audio["audio_base64"]);
    assert_eq!(
        synthesized(&voice).await.len(),
        rendered + 1,
        "a held replay is never rendered again"
    );
    core.played(
        &token,
        &session,
        &held,
        resumed["revision"].as_u64().unwrap(),
    )
    .await;

    // A replay already delivered when the user talks over it is heard again after the turn.
    let delivered = replay().await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    let delivered = delivered.json()["utterance_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        browser.frame("voice-replay").await["replies"][0]["utterance_id"],
        delivered.as_str()
    );
    let first = browser
        .frame_within("voice-speech-audio", Duration::from_secs(20))
        .await;
    assert_eq!(first["audio_base64"], audio["audio_base64"]);
    let microphone = browser.microphone();
    let talking = {
        let pcm = pcm.clone();
        tokio::spawn(async move { microphone.speak(&pcm).await })
    };
    browser.turn("started", STEP).await;
    talking.await.unwrap();
    browser.speak(&silence(4.0)).await;
    let ask = browser
        .frame_within("voice-transcribe", Duration::from_secs(30))
        .await;
    browser
        .transcript(&ask, "Resume the delivered replay")
        .await;
    browser.turn("finished", STEP).await;
    let resumed = browser
        .frame_within("voice-speech-audio", Duration::from_secs(20))
        .await;
    assert_eq!(resumed["utterance_id"], delivered.as_str());
    assert_eq!(resumed["audio_base64"], audio["audio_base64"]);
    assert_eq!(
        synthesized(&voice).await.len(),
        rendered + 1,
        "a delivered replay is never rendered again"
    );
    core.played(
        &token,
        &session,
        &delivered,
        resumed["revision"].as_u64().unwrap(),
    )
    .await;

    // Input the browser cancels while the user is still talking leaves no turn behind.
    let microphone = browser.microphone();
    let talking = {
        let pcm = pcm.clone();
        tokio::spawn(async move { microphone.speak(&pcm).await })
    };
    let started = browser.turn("started", STEP).await;
    let cancel = || {
        core.post(
            "/api/presentation/cancel-input",
            json!({"session_id": session, "revision": started["revision"]}),
        )
        .token(&token)
        .send()
    };
    let cancelled = cancel().await;
    assert_eq!(cancelled.status, 200, "{cancelled:?}");
    assert_eq!(cancelled.json()["status"], "cancelled");
    talking.await.unwrap();
    let turn = browser.turn("cancelled", STEP).await;
    assert_eq!(turn["revision"], started["revision"]);
    assert_eq!(cancel().await.status, 409);
    let history = core.history(&token, THREAD).await;
    let gone = format!("{session}:user-turn:{}", started["revision"]);
    assert!(
        history.iter().all(|row| row["id"] != gone.as_str()),
        "{history:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pauses_inside_the_merge_window_make_one_turn() {
    let _serial = serial().await;
    let call = call(|_, launch| launch).await;
    let (core, token) = (&call.core, call.token.clone());
    let pcm = speech();
    let mut browser = core.join(&token, timer(3.0)).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    for part in ["First thought", "continued thought"] {
        browser.speak(&pcm).await;
        browser.speak(&silence(1.0)).await;
        let ask = browser
            .frame_within("voice-transcribe", Duration::from_secs(30))
            .await;
        browser.transcript(&ask, part).await;
    }
    let merged = loop {
        let turn = browser.frame("voice-user-turn").await;
        if turn["phase"] == "finished" {
            break turn;
        }
        if turn["phase"] != "started" {
            assert_eq!(turn["merged"], true, "{turn}");
        }
    };
    assert_eq!(merged["text"], "First thought continued thought");
    assert_eq!(
        browser.receipt("pending").await["revision"],
        merged["revision"]
    );

    // A second utterance inside the window while the first is still being transcribed joins it.
    let mut browser = core.join(&token, timer(2.0)).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    browser.speak(&pcm).await;
    browser.speak(&silence(2.0)).await;
    let first = browser
        .frame_within("voice-transcribe", Duration::from_secs(20))
        .await;
    browser.speak(&pcm).await;
    browser.speak(&silence(2.0)).await;
    browser
        .none_of(&["voice-transcribe"], Duration::from_millis(400))
        .await;
    browser.transcript(&first, "First spoken").await;
    let second = browser
        .frame_within("voice-transcribe", Duration::from_secs(20))
        .await;
    browser.transcript(&second, "Second spoken").await;
    let turn = browser.turn("finished", STEP).await;
    assert_eq!(turn["text"], "First spoken Second spoken");
    browser.receipt("pending").await;
    browser.speak(&pcm).await;
    browser.speak(&silence(2.0)).await;
    browser
        .frame_within("voice-transcribe", Duration::from_secs(20))
        .await;
    browser.close().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let own = core.history(&token, THREAD).await;
    assert_eq!(
        own.iter()
            .filter(|row| row["session"] == session.as_str())
            .count(),
        1,
        "{own:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_speech_arrives_as_one_turn_and_failed_transcriptions_cancel_theirs() {
    let _serial = serial().await;
    let call = call(|_, launch| launch.env("SIDEVOICE_FIXTURE_STT_TIMEOUT_MS", "12000")).await;
    let (core, token) = (&call.core, call.token.clone());
    let pcm = speech();
    let mut browser = core.join(&token, timer(0.0)).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    let revision = core.revision(&token, &session).await;

    // What the browser recorded while it was offline, in two parts.
    let started = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64)
        - 1000;
    for (seq, part) in [&pcm[..40_000], &pcm[40_000..]].into_iter().enumerate() {
        browser
            .send(
                "voice-catchup",
                json!({"session_id": session, "sample_rate": 16000, "seq": seq,
                    "audio_base64": base64::engine::general_purpose::STANDARD.encode(part),
                    "started_at": started, "final": seq == 1}),
            )
            .await;
    }
    let ask = browser
        .frame_within("voice-transcribe", Duration::from_secs(15))
        .await;
    let wav = hound::WavReader::new(std::io::Cursor::new(decode(&ask["audio_base64"]))).unwrap();
    assert_eq!(wav.spec().sample_rate, 16000);
    browser.transcript(&ask, "Words recorded offline").await;
    let catchup = browser.frame("voice-catchup-turn").await;
    assert_eq!(catchup["history_id"], format!("{session}:user-catchup:1"));
    assert_eq!(catchup["time"], started);
    assert_eq!(catchup["offline"], "buffered");
    assert_eq!(catchup["thread_id"], THREAD);
    assert_eq!(browser.receipt("pending").await["revision"], 0);
    let rows = core.history(&token, THREAD).await;
    let row = rows.last().unwrap();
    assert_eq!(
        (
            row["offline"].clone(),
            row["time"].clone(),
            row["revision"].clone()
        ),
        (json!("buffered"), json!(started), json!(0))
    );
    assert_eq!(
        core.revision(&token, &session).await,
        revision,
        "offline speech does not move the call"
    );
    // A second catch-up that repeats a part already taken is not transcribed again.
    for seq in [0, 2] {
        browser
            .send(
                "voice-catchup",
                json!({"session_id": session, "sample_rate": 16000, "seq": seq,
                    "audio_base64": base64::engine::general_purpose::STANDARD.encode(&pcm[..16_000]),
                    "final": seq == 2}),
            )
            .await;
    }
    browser
        .none_of(&["voice-transcribe"], Duration::from_millis(700))
        .await;
    let own = core.history(&token, THREAD).await;
    assert_eq!(
        own.iter()
            .filter(|row| row["session"] == session.as_str())
            .count(),
        1
    );

    // The browser fails to transcribe, or never answers: the turn is cancelled and nothing reaches the
    // conversation.
    for cause in ["device", "timeout"] {
        browser.speak(&pcm).await;
        browser.speak(&silence(4.0)).await;
        let ask = browser
            .frame_within("voice-transcribe", Duration::from_secs(20))
            .await;
        if cause == "device" {
            browser
                .send(
                    "voice-transcript-error",
                    json!({"session_id": session, "request_id": ask["request_id"], "error": "fixture failure"}),
                )
                .await;
        }
        let error = browser.frame_within("error", Duration::from_secs(16)).await;
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "{error}"
        );
        let cancelled = browser.frame_within("voice-user-turn", STEP).await;
        assert_eq!(
            (cancelled["phase"].as_str(), cancelled["text"].as_str()),
            (Some("cancelled"), Some("")),
            "{cancelled}"
        );
    }
    let own = core.history(&token, THREAD).await;
    assert_eq!(
        own.iter()
            .filter(|row| row["session"] == session.as_str())
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_keeps_the_focus_it_started_with_while_both_listeners_hear_the_reply() {
    let _serial = serial().await;
    let call = call(|_, launch| launch).await;
    let other = call
        .peer
        .call("binding.register", registration("other", OTHER))
        .await
        .data;
    assert!(other["binding_id"].is_string(), "{other}");
    let (core, token) = (&call.core, call.token.clone());
    let pcm = speech();
    let mut first = core.join(&token, timer(0.0)).await;
    let mut second = core.join(&token, timer(0.0)).await;
    for browser in [&first, &second] {
        core.select(&token, &browser.session, THREAD).await;
    }
    let revision = core.revision(&token, &first.session).await;
    let uid = format!("both-{}", message_id());
    let answer = publish(
        &call.peer,
        &call.binding,
        &first.session,
        revision,
        &uid,
        "Reply to both listeners",
    )
    .await;
    assert_eq!(answer["status"], "queued");
    let speeches = [
        first.frame("voice-speech").await,
        second.frame("voice-speech").await,
    ];
    for (browser, speech) in [&first, &second].into_iter().zip(&speeches) {
        assert_eq!(speech["session_id"], browser.session.as_str());
        assert_eq!(speech["utterance_id"], uid.as_str());
    }
    for status in ["playing", "playback_finished"] {
        for (browser, speech) in [&first, &second].into_iter().zip(&speeches) {
            let revision = speech["revision"].as_u64().unwrap();
            assert_eq!(
                core.receipt(&token, &browser.session, &uid, revision, status)
                    .await,
                200
            );
        }
    }

    // The focus moves while the first browser is still speaking: the turn under way keeps its conversation,
    // the next one goes to the new focus.
    let microphone = first.microphone();
    let talking = {
        let mut audio = pcm.clone();
        audio.extend_from_slice(&pcm);
        audio.extend(silence(2.0));
        tokio::spawn(async move { microphone.speak(&audio).await })
    };
    let started = first.turn("started", Duration::from_secs(15)).await;
    assert_eq!(started["thread_id"], THREAD);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    core.select(&token, &first.session, OTHER).await;
    talking.await.unwrap();
    let old = first
        .frame_within("voice-transcribe", Duration::from_secs(20))
        .await;
    first.transcript(&old, "Before the focus moved").await;
    let old_turn = first.turn("finished", STEP).await;
    assert_eq!(
        (old_turn["thread_id"].as_str(), old_turn["text"].as_str()),
        (Some(THREAD), Some("Before the focus moved"))
    );
    let new = first
        .frame_within("voice-transcribe", Duration::from_secs(20))
        .await;
    assert_ne!(new["request_id"], old["request_id"]);
    first.transcript(&new, "After the focus moved").await;
    let new_turn = first.turn("finished", Duration::from_secs(18)).await;
    assert_eq!(
        (new_turn["thread_id"].as_str(), new_turn["text"].as_str()),
        (Some(OTHER), Some("After the focus moved"))
    );
    let session = first.session.clone();
    assert!(core
        .history(&token, THREAD)
        .await
        .iter()
        .any(|row| row["text"] == "Before the focus moved" && row["session"] == session.as_str()));
    assert!(core
        .history(&token, OTHER)
        .await
        .iter()
        .any(|row| row["text"] == "After the focus moved" && row["session"] == session.as_str()));
    second.close().await;
}

/// A reply the browser reports playing; its utterance and revision.
async fn reply_playing(call: &Call, browser: &mut Browser) -> (String, u64) {
    let (core, token, session) = (&call.core, &call.token, browser.session.clone());
    let uid = format!("playing-{}", message_id());
    let revision = core.revision(token, &session).await;
    let answer = publish(
        &call.peer,
        &call.binding,
        &session,
        revision,
        &uid,
        "Speaker output",
    )
    .await;
    assert_eq!(answer["status"], "queued");
    let speech = browser.frame("voice-speech").await;
    let revision = speech["revision"].as_u64().unwrap();
    assert_eq!(
        core.receipt(token, &session, &uid, revision, "playing")
            .await,
        200
    );
    (uid, revision)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_reply_playing_is_not_heard_as_the_user_but_a_louder_voice_interrupts_it() {
    let _serial = serial().await;
    let call = call(|_, launch| launch).await;
    let (core, token) = (&call.core, call.token.clone());
    let pcm = speech();
    let mut browser = core.join(&token, timer(0.0)).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;

    let (uid, revision) = reply_playing(&call, &mut browser).await;
    browser.speak(&pcm).await;
    browser.speak(&silence(2.0)).await;
    browser
        .none_of(&["voice-transcribe"], Duration::from_millis(700))
        .await;
    assert_eq!(
        core.receipt(&token, &session, &uid, revision, "playback_finished")
            .await,
        200
    );
    browser.speak(&pcm).await;
    browser.speak(&silence(2.0)).await;
    let ask = browser
        .frame_within("voice-transcribe", Duration::from_secs(20))
        .await;
    browser
        .transcript(&ask, "Normal voice after playback")
        .await;
    browser.turn("finished", STEP).await;
    browser.receipt("pending").await;

    let (uid, revision) = reply_playing(&call, &mut browser).await;
    let loud: Vec<u8> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|pair| {
            let sample = i16::from_le_bytes(*pair) as i32 * 8;
            (sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_le_bytes()
        })
        .collect();
    browser.speak(&loud).await;
    browser.speak(&silence(2.0)).await;
    browser
        .frame_within("voice-cancel", Duration::from_secs(15))
        .await;
    let ask = browser
        .frame_within("voice-transcribe", Duration::from_secs(20))
        .await;
    assert_eq!(
        core.receipt(&token, &session, &uid, revision, "playing")
            .await,
        409
    );
    browser.transcript(&ask, "Louder barge in").await;
    browser.turn("finished", STEP).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_spoken_over_webrtc_then_over_the_socket_after_a_replaced_offer() {
    let _serial = serial().await;
    let call = call(|_, launch| launch).await;
    let (core, token) = (&call.core, call.token.clone());
    let pcm = speech();
    let mut browser = core.join(&token, timer(0.0)).await;
    let session = browser.session.clone();
    core.select(&token, &session, THREAD).await;
    let before = core.history(&token, THREAD).await.len();

    let rtc = RtcBrowser::new().await;
    rtc.connect(core, &token, &session).await;
    browser
        .send(
            "voice-media",
            json!({"session_id": session, "path": "webrtc"}),
        )
        .await;
    let mut audio = pcm.clone();
    audio.extend(silence(4.0));
    rtc.speak(&audio).await;
    transcribed_turn(&mut browser, "Hola over WebRTC").await;

    // A new offer replaces the first peer; the browser falls back to the socket and is heard there.
    let replacement = RtcBrowser::new().await;
    let (status, answer) = replacement.offer_to(core, &token, &session).await;
    assert_eq!(status, 200, "{answer}");
    browser
        .send(
            "voice-media",
            json!({"session_id": session, "path": "socket"}),
        )
        .await;
    spoken_turn(&mut browser, &pcm, "Hola over the socket").await;
    let after = core.history(&token, THREAD).await;
    let own = after
        .iter()
        .filter(|row| row["role"] == "user" && row["session"] == session.as_str())
        .count();
    assert_eq!((after.len(), own), (before + 2, 2), "{after:?}");

    // Unpairing the app ends its call.
    let revoked = core.local("DELETE", "/api/device/local").send().await;
    assert_eq!(revoked.status, 200, "{revoked:?}");
    assert_eq!(revoked.json()["revoked"], true);
    assert_eq!(browser.closed(Duration::from_secs(8)).await, 4401);
    rtc.close().await;
    replacement.close().await;
}
