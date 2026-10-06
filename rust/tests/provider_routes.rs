//! The routes a page uses before and around a call that reach a cloud provider: the first call's preflight, the
//! provider keys and what they unlock (transcription models, the voice catalogue), the transcription trial with
//! its limits, the synthesis preview and the model checks. The providers are local fakes the core is pointed at
//! through the `hosted-fixtures` overrides; nothing leaves the machine.

mod support;

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use support::*;
use wiremock::matchers::{body_string_contains, header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const ORIGIN: &str = "tauri://localhost";
const STORED: &str = "stored-openai-fixture-key";
const ENVIRONMENT: &str = "environment-elevenlabs-fixture-key";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/rust_t4")
            .join(name),
    )
    .unwrap()
}

/// Five seconds of a 220 Hz tone, 16 kHz mono PCM: audible, and long enough for any check.
fn tone() -> Vec<u8> {
    (0..5 * 16_000)
        .flat_map(|i| {
            let phase = 2.0 * std::f64::consts::PI * 220.0 * f64::from(i) / 16_000.0;
            ((6000.0 * phase.sin()) as i16).to_le_bytes()
        })
        .collect()
}

/// OpenAI and ElevenLabs as far as these routes reach them.
async fn providers() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer bad-key"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "rejected"}})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("xi-api-key", ENVIRONMENT))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("eleven_models.json"), "application/json"),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": [
            {"id": "gpt-4o-transcribe", "object": "model", "created": 0, "owned_by": "fixture"},
            {"id": "ordinary-model", "object": "model", "created": 0, "owned_by": "fixture"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/v2/voices"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("eleven_voices_sparse.json"), "application/json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(body_string_contains("gpt-slow-transcribe"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"text": "Hello from the provider"}))
                .set_delay(Duration::from_secs(3)),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"text": "Hello from the provider"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("^/v1/text-to-speech/"))
        .and(query_param("output_format", "pcm_16000"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(tone(), "audio/pcm"))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("^/v1/text-to-speech/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"ID3fixture".to_vec(), "audio/mpeg"))
        .mount(&server)
        .await;
    server
}

fn transcriptions(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.url.path() == "/v1/audio/transcriptions")
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_provider_routes_take_keys_list_what_they_unlock_and_bound_their_trials() {
    let server = providers().await;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&data)
        .unwrap();
    let integrations = data.join("integrations.json");
    std::fs::write(&integrations, json!({"openai": STORED}).to_string()).unwrap();
    std::fs::set_permissions(&integrations, std::fs::Permissions::from_mode(0o600)).unwrap();
    let core = Launch::new(&data)
        .env("VOICE_ELEVENLABS_API_KEY", ENVIRONMENT)
        .env(
            "SIDEVOICE_OPENAI_FIXTURE_BASE",
            format!("{}/v1", server.uri()),
        )
        .env("SIDEVOICE_ELEVENLABS_FIXTURE_BASE", server.uri())
        .start();

    // The first call: the page asks for the defaults with its token and joins with them as they are.
    assert_eq!(
        core.get("/api/presentation/languages").send().await.status,
        401
    );
    assert_eq!(
        core.get("/api/presentation/languages")
            .token("wrong")
            .send()
            .await
            .status,
        401
    );
    let token = core.pair_local("Browser").await;
    let preferences = core
        .get("/api/presentation/languages")
        .token(&token)
        .origin(ORIGIN)
        .send()
        .await;
    assert_eq!(preferences.status, 200, "{preferences:?}");
    let preferences = preferences.json();
    assert!(
        preferences["stt"]["build"].is_null() && preferences["tts"]["build"].is_null(),
        "{preferences}"
    );
    let browser = core.join(&token, preferences.clone()).await;
    assert_eq!(browser.welcome["sample_rate"], 16000);
    browser.close().await;

    // Keys: where each comes from, never the key itself.
    let listing = core
        .get("/api/presentation/integrations")
        .token(&token)
        .origin(ORIGIN)
        .send()
        .await;
    assert_eq!(listing.status, 200);
    let raw = String::from_utf8_lossy(&listing.body).into_owned();
    assert!(!raw.contains(STORED) && !raw.contains(ENVIRONMENT), "{raw}");
    let rows: Vec<_> = listing.json()["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["id"].clone(),
                row["source"].clone(),
                row["configured"].clone(),
                row["hint"].clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                json!("openai"),
                json!("stored"),
                json!(true),
                json!("…-key")
            ),
            (
                json!("elevenlabs"),
                json!("environment"),
                json!(true),
                json!("…-key")
            ),
        ]
    );

    // What the keys unlock.
    let models = core
        .get("/api/presentation/transcription/models?provider=openai")
        .token(&token)
        .origin(ORIGIN)
        .send()
        .await;
    assert_eq!(models.status, 200, "{models:?}");
    let ids: Vec<_> = models.json()["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].clone())
        .collect();
    assert_eq!(
        ids,
        [json!("gpt-4o-transcribe")],
        "only transcription models are offered"
    );
    let catalog = core
        .get("/api/presentation/voice-catalog")
        .token(&token)
        .origin(ORIGIN)
        .send()
        .await;
    assert_eq!(catalog.status, 200, "{catalog:?}");
    let catalog = catalog.json();
    assert!(catalog["languages"]
        .as_array()
        .is_some_and(|languages| !languages.is_empty()));
    let elevenlabs = &catalog["providers"]["elevenlabs"];
    assert_eq!(elevenlabs["configured"], true);
    assert!(
        elevenlabs["models"]
            .as_array()
            .is_some_and(|models| !models.is_empty()),
        "{elevenlabs}"
    );
    assert!(
        elevenlabs["voices"]
            .as_array()
            .is_some_and(|voices| !voices.is_empty()),
        "{elevenlabs}"
    );

    // The transcription trial: a provider without a key is unavailable; a request the trial cannot run never
    // reaches the provider; one at a time per device, six a minute.
    let trial = json!({"place": "openai", "model": "gpt-4o-transcribe",
        "options": {"language": "auto", "context": "fixture context"},
        "audio": {"encoding": "pcm_s16le", "sample_rate": 16000,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(tone())}});
    let preview = |body: Value| {
        core.post("/api/models/transcription/preview", body)
            .token(&token)
            .origin(ORIGIN)
            .send()
    };
    let cleared = core
        .http("DELETE", "/api/presentation/integrations/openai")
        .token(&token)
        .origin(ORIGIN)
        .send()
        .await;
    assert_eq!(cleared.status, 200, "{cleared:?}");
    let unavailable = preview(trial.clone()).await;
    assert_eq!(
        (unavailable.status, unavailable.key()),
        (409, "trial.provider_unavailable".into())
    );
    let saved = core
        .http("PUT", "/api/presentation/integrations/openai")
        .token(&token)
        .origin(ORIGIN)
        .json(json!({"key": "good-key"}))
        .send()
        .await;
    assert_eq!(saved.status, 200, "{saved:?}");
    assert_eq!(
        core.post("/api/models/transcription/preview", trial.clone())
            .send()
            .await
            .status,
        401
    );
    assert_eq!(
        core.post("/api/models/transcription/preview", trial.clone())
            .token(&token)
            .origin("https://foreign.example")
            .send()
            .await
            .status,
        403
    );
    let reached = transcriptions(&server.received_requests().await.unwrap()).len();
    let mut wav = trial.clone();
    wav["audio"]["encoding"] = json!("wav");
    let mut garbled = trial.clone();
    garbled["audio"]["data_base64"] = json!("bad?");
    let mut unknown = trial.clone();
    unknown["model"] = json!("invalid model");
    for (body, status, key) in [
        (unknown, 422, "trial.invalid_stage"),
        (wav, 400, "trial.invalid_audio"),
        (garbled, 400, "trial.invalid_audio"),
    ] {
        let refused = preview(body).await;
        assert_eq!(
            (refused.status, refused.key()),
            (status, key.to_owned()),
            "{refused:?}"
        );
    }
    assert_eq!(
        transcriptions(&server.received_requests().await.unwrap()).len(),
        reached
    );
    let history = core
        .get("/api/presentation/history")
        .token(&token)
        .send()
        .await
        .json();

    let mut slow = trial.clone();
    slow["model"] = json!("gpt-slow-transcribe");
    let running = tokio::spawn(preview(slow));
    eventually(STEP, "the slow trial to reach the provider", || {
        let server = &server;
        async move {
            let requests = server.received_requests().await.unwrap_or_default();
            transcriptions(&requests)
                .iter()
                .any(|body| body.contains("gpt-slow-transcribe"))
                .then_some(())
        }
    })
    .await;
    let busy = preview(trial.clone()).await;
    assert_eq!((busy.status, busy.key()), (429, "trial.busy".into()));
    assert_eq!(running.await.unwrap().status, 200);
    for _ in 0..5 {
        let done = preview(trial.clone()).await;
        assert_eq!(done.status, 200, "{done:?}");
        assert!(done.json()["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty()));
    }
    let sent = transcriptions(&server.received_requests().await.unwrap());
    assert_eq!(sent.len(), reached + 6);
    assert!(sent[reached..]
        .iter()
        .all(|body| body.contains("fixture context") && body.contains("RIFF")));
    let limited = preview(trial.clone()).await;
    assert_eq!((limited.status, limited.key()), (429, "trial.busy".into()));
    assert_eq!(
        core.get("/api/presentation/history")
            .token(&token)
            .send()
            .await
            .json(),
        history,
        "a trial leaves nothing in the room"
    );

    // The synthesis preview and the model checks go through the same keys.
    let spoken = core
        .post(
            "/api/presentation/synthesis/preview",
            json!({"text": "Hello", "model": "eleven_multilingual_v2", "voice": "sparse-voice", "speed": 1}),
        )
        .token(&token)
        .origin(ORIGIN)
        .send()
        .await;
    assert_eq!(spoken.status, 200, "{spoken:?}");
    assert!(spoken.json()["audio_base64"]
        .as_str()
        .is_some_and(|audio| !audio.is_empty()));
    let check = |body: Value| {
        core.post("/api/models/check", body)
            .token(&token)
            .origin(ORIGIN)
            .send()
    };
    let on_device = check(json!({"stage": "stt", "place": "device"})).await;
    assert_eq!(
        (on_device.status, on_device.key()),
        (400, "check_on_device".into())
    );
    let voice = check(
        json!({"stage": "tts", "place": "elevenlabs", "model": "eleven_multilingual_v2",
        "options": {"voice": {"en": "sparse-voice"}, "speed": 1}, "language": "en"}),
    )
    .await;
    assert_eq!(voice.status, 200, "{voice:?}");
    assert_eq!(voice.json()["ok"], true, "{:?}", voice.json());
}
