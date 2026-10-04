//! Bounded paid provider tries, using the shared catalogue verdicts and SDK adapters.

use super::*;
use crate::providers::{ElevenLabsTts, OpenAiTranscriber, ProviderError, ProviderErrorKind};
use crate::types::SpeechStage;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::watch;

const WINDOW: Duration = Duration::from_secs(60);
const REMEMBER: Duration = Duration::from_secs(600);

#[derive(Default)]
pub(super) struct CheckBudget {
    inner: Mutex<BudgetState>,
}

#[derive(Default)]
struct BudgetState {
    windows: HashMap<(String, String), VecDeque<Instant>>,
    passed: HashMap<String, (Instant, Value)>,
    running: HashMap<String, watch::Sender<Option<Value>>>,
}

enum Admission {
    Cached(Value),
    Join(watch::Receiver<Option<Value>>),
    Start(watch::Receiver<Option<Value>>),
    Limited(u64, &'static str),
}

impl CheckBudget {
    fn admit(&self, key: &str, device: &str, provider: &str) -> Admission {
        let now = Instant::now();
        let mut state = self.inner.lock().expect("check budget lock");
        state
            .passed
            .retain(|_, (at, _)| now.duration_since(*at) < REMEMBER);
        if let Some((_, result)) = state.passed.get(key) {
            let mut remembered = result.clone();
            remembered["remembered"] = json!(true);
            return Admission::Cached(remembered);
        }
        if let Some(running) = state.running.get(key) {
            return Admission::Join(running.subscribe());
        }
        for (scope, name, limit) in [("device", device, 6), ("provider", provider, 12)] {
            let window = state
                .windows
                .entry((scope.to_owned(), name.to_owned()))
                .or_default();
            while window
                .front()
                .is_some_and(|at| now.duration_since(*at) >= WINDOW)
            {
                window.pop_front();
            }
            if window.len() >= limit {
                let wait = WINDOW
                    .saturating_sub(now.duration_since(*window.front().expect("full window")));
                return Admission::Limited(
                    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1),
                    scope,
                );
            }
        }
        for (scope, name) in [("device", device), ("provider", provider)] {
            state
                .windows
                .entry((scope.to_owned(), name.to_owned()))
                .or_default()
                .push_back(now);
        }
        let (sender, receiver) = watch::channel(None);
        state.running.insert(key.to_owned(), sender);
        Admission::Start(receiver)
    }

    fn complete(&self, key: &str, result: Value) {
        let mut state = self.inner.lock().expect("check budget lock");
        if result["ok"] == true {
            state
                .passed
                .insert(key.to_owned(), (Instant::now(), result.clone()));
        }
        if let Some(sender) = state.running.remove(key) {
            let _ = sender.send(Some(result));
        }
    }
}

fn refusal(message: LocalizedMessage, language: &str) -> Value {
    Value::Object(crate::messages::render_refusal(&message, language))
}

fn invalid_stage(language: &str) -> Value {
    let details = render(&LocalizedMessage::new("settings.stage_invalid"), language);
    refusal(
        LocalizedMessage::new("check_invalid").with_param("details", details),
        language,
    )
}

fn failed(step: &str, reason: Value, passes: Vec<Value>) -> Value {
    json!({"ok":false,"step":step,"reason":reason,"passes":passes})
}

fn provider_problem(place: &str, error: &ProviderError, language: &str) -> (&'static str, Value) {
    let label = if place == "openai" {
        "OpenAI"
    } else {
        "ElevenLabs"
    };
    let (step, message) = match error.kind {
        ProviderErrorKind::Unauthorized => ("key", LocalizedMessage::new("provider_key_refused")),
        ProviderErrorKind::Timeout | ProviderErrorKind::Transport => {
            ("check", LocalizedMessage::new("provider_unreachable"))
        }
        _ => (
            "check",
            LocalizedMessage::new("provider_failed").with_param(
                "detail",
                error
                    .status
                    .map_or_else(|| format!("{:?}", error.kind), |status| status.to_string()),
            ),
        ),
    };
    (
        step,
        refusal(
            message
                .with_param("provider", place.to_owned())
                .with_param("provider_label", label),
            language,
        ),
    )
}

fn check_key(task: &str, stage: &SpeechStage, language: Option<&str>, credential: &str) -> String {
    let digest = Sha256::digest(credential.as_bytes());
    let revision = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    json!([
        task,
        stage.place,
        stage.model,
        stage.options,
        language,
        revision
    ])
    .to_string()
}

async fn check_provider(
    task: &str,
    stage: SpeechStage,
    language: Option<String>,
    key: String,
    ui: String,
) -> Value {
    if task == "stt" {
        let own = stage
            .options
            .get("language")
            .and_then(Value::as_str)
            .filter(|value| *value != "auto");
        let chosen = crate::models::check_language("stt", own.or(language.as_deref()));
        let clip = crate::models::stt_check_clip(Some(chosen));
        let client = match OpenAiTranscriber::new(&key) {
            Ok(client) => client,
            Err(error) => {
                let (step, reason) = provider_problem(&stage.place, &error, &ui);
                return failed(step, reason, vec![]);
            }
        };
        let mut passes = Vec::new();
        for _ in 0..2 {
            let start = Instant::now();
            let result = client
                .transcribe(
                    clip.audio,
                    &stage.model,
                    Some(chosen),
                    stage
                        .options
                        .get("context")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty()),
                )
                .await;
            let transcript = match result {
                Ok(transcript) => transcript,
                Err(error) => {
                    let (step, reason) = provider_problem(&stage.place, &error, &ui);
                    return failed(step, reason, passes);
                }
            };
            let latency = start.elapsed().as_millis() as u64;
            passes.push(json!({"latency_ms":latency,"text":transcript.text}));
            if let Some(problem) = crate::models::transcript_problem(clip.text, &transcript.text) {
                return failed("check", refusal(problem, &ui), passes);
            }
        }
        let latency = passes
            .last()
            .and_then(|pass| pass["latency_ms"].as_u64())
            .unwrap_or(0);
        return json!({"ok":true,"step":"done","language":chosen,"passes":passes,
            "latency_ms":latency,"slow":crate::models::slow(latency)});
    }
    let chosen = crate::models::check_language("tts", language.as_deref());
    let voice = stage.options.get("voice").and_then(|value| match value {
        Value::Object(voices) => voices
            .get(chosen)
            .or_else(|| voices.values().next())
            .and_then(Value::as_str),
        _ => value.as_str(),
    });
    let Some(voice) = voice.filter(|voice| !voice.is_empty()) else {
        return failed(
            "check",
            refusal(
                LocalizedMessage::new("voice_missing")
                    .with_param("provider", stage.place)
                    .with_param("provider_label", "ElevenLabs"),
                &ui,
            ),
            vec![],
        );
    };
    let client = match ElevenLabsTts::new(&key) {
        Ok(client) => client,
        Err(error) => {
            let (step, reason) = provider_problem(&stage.place, &error, &ui);
            return failed(step, reason, vec![]);
        }
    };
    let phrase = crate::models::tts_check_phrase(Some(chosen));
    let speed = stage
        .options
        .get("speed")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let mut passes = Vec::new();
    for _ in 0..2 {
        let result = client
            .synthesize(phrase, &stage.model, voice, speed, false, "pcm_16000")
            .await;
        let speech = match result {
            Ok(speech) => speech,
            Err(error) => {
                let (step, reason) = provider_problem(&stage.place, &error, &ui);
                return failed(step, reason, passes);
            }
        };
        let samples = speech
            .audio
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32768.0)
            .collect::<Vec<_>>();
        let total = speech
            .timings_ms
            .get("request_to_complete_ms")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let first = speech
            .timings_ms
            .get("request_to_first_chunk_ms")
            .and_then(Value::as_f64)
            .unwrap_or(total);
        let seconds = samples.len() as f64 / 16_000.0;
        passes.push(
            json!({"first_audio_ms":first.round() as u64,"total_ms":total.round() as u64,
            "audio_seconds":(seconds*100.0).round()/100.0,
            "realtime":if total>0.0 {Some((seconds*100_000.0/total).round()/100.0)} else {None}}),
        );
        if let Some(problem) = crate::models::audio_problem(&samples, 16_000.0) {
            return failed("check", refusal(problem, &ui), passes);
        }
    }
    let latency = passes
        .last()
        .and_then(|pass| pass["first_audio_ms"].as_u64())
        .unwrap_or(0);
    json!({"ok":true,"step":"done","language":chosen,"voice":voice,"passes":passes,
        "latency_ms":latency,"slow":false})
}

pub(super) async fn model_check(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    let ui = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("en")
        .to_owned();
    let task = data["stage"].as_str().unwrap_or("");
    if !matches!(task, "stt" | "tts") {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":invalid_stage(&ui)})),
        )
            .into_response();
    }
    let place = data["place"].as_str().unwrap_or("");
    if place == "host" {
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail":refusal(LocalizedMessage::new("place_host_unavailable"),&ui)})),
        )
            .into_response();
    }
    if place == "device" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":refusal(LocalizedMessage::new("check_on_device"),&ui)})),
        )
            .into_response();
    }
    let stage_input = json!({"place":place,"model":data["model"],"options":data.get("options").filter(|value|value.is_object()).cloned().unwrap_or_else(||json!({}))});
    let Some(stage) = crate::models::provider_check_stage(task, &stage_input) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":invalid_stage(&ui)})),
        )
            .into_response();
    };
    let language = data["language"].as_str().map(str::to_owned);
    let Some(key) = media::provider_key(&state.dir, &stage.place) else {
        let label = if stage.place == "openai" {
            "OpenAI"
        } else {
            "ElevenLabs"
        };
        return Json(json!({"stage":task,"place":stage.place,"model":stage.model,
            "ok":false,"step":"key","reason":refusal(LocalizedMessage::new("provider_key_missing")
                .with_param("provider",stage.place.clone()).with_param("provider_label",label),&ui),"passes":[]})).into_response();
    };
    let budget_key = check_key(task, &stage, language.as_deref(), &key);
    let admission = state
        .check_budget
        .admit(&budget_key, &device.0, &stage.place);
    let mut receiver = match admission {
        Admission::Cached(value) => {
            let mut response = json!({"stage":task,"place":stage.place,"model":stage.model});
            if let Some(fields) = value.as_object() {
                for (name, field) in fields {
                    response[name] = field.clone();
                }
            }
            return Json(response).into_response();
        }
        Admission::Limited(wait, scope) => {
            let reason = refusal(
                LocalizedMessage::new("check_rate_limited")
                    .with_param("retry_after", wait)
                    .with_param("scope", scope),
                &ui,
            );
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"detail":reason})),
            )
                .into_response();
            if let Ok(value) = HeaderValue::from_str(&wait.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            return response;
        }
        Admission::Join(receiver) => receiver,
        Admission::Start(receiver) => {
            let budget = state.clone();
            let key_for_task = budget_key.clone();
            let task_for_work = task.to_owned();
            let ui_for_work = ui.clone();
            tokio::spawn(async move {
                let answer =
                    check_provider(&task_for_work, stage, language, key, ui_for_work).await;
                budget.check_budget.complete(&key_for_task, answer);
            });
            receiver
        }
    };
    if receiver.changed().await.is_err() {
        return failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers);
    }
    let Some(result) = receiver.borrow().clone() else {
        return failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers);
    };
    let mut response = json!({"stage":task,"place":place,"model":data["model"]});
    if let Some(fields) = result.as_object() {
        for (name, value) in fields {
            response[name] = value.clone();
        }
    }
    Json(response).into_response()
}
