//! One bounded, non-room transcription of a selected device's spoken sample.

use super::*;
use crate::providers::{OpenAiTranscriber, ProviderErrorKind};
use axum::body::Bytes;
use base64::Engine as _;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_BODY_BYTES: usize = 512 * 1024;
const MAX_PCM_BYTES: usize = 10 * 16_000 * 2;
const MIN_PCM_BYTES: usize = 16_000 / 4 * 2;
const LIMIT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Default, Clone)]
pub(super) struct TrialBudget {
    inner: Arc<Mutex<HashMap<String, TrialWindow>>>,
}

#[derive(Default)]
struct TrialWindow {
    active: bool,
    starts: VecDeque<Instant>,
}

struct TrialLease {
    budget: TrialBudget,
    device: String,
}

impl Drop for TrialLease {
    fn drop(&mut self) {
        if let Some(window) = self.budget.inner.lock().expect("trial budget lock").get_mut(&self.device) {
            window.active = false;
        }
    }
}

impl TrialBudget {
    fn start(&self, device: &str) -> Result<TrialLease, u64> {
        let now = Instant::now();
        let mut windows = self.inner.lock().expect("trial budget lock");
        windows.retain(|_, window| window.active || window.starts.back().is_some_and(|at| now.duration_since(*at) < LIMIT_WINDOW));
        let window = windows.entry(device.to_owned()).or_default();
        while window.starts.front().is_some_and(|at| now.duration_since(*at) >= LIMIT_WINDOW) {
            window.starts.pop_front();
        }
        if window.active {
            return Err(1);
        }
        if window.starts.len() >= 6 {
            let wait = LIMIT_WINDOW.saturating_sub(now.duration_since(*window.starts.front().expect("full window")));
            return Err((wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1));
        }
        window.active = true;
        window.starts.push_back(now);
        Ok(TrialLease { budget: self.clone(), device: device.to_owned() })
    }
}

fn fail(key: &str, status: StatusCode, headers: &HeaderMap) -> Response {
    let language = headers.get(header::ACCEPT_LANGUAGE).and_then(|value| value.to_str().ok()).unwrap_or("en");
    let refusal = crate::messages::render_refusal(&LocalizedMessage::new(key), language);
    (status, Json(json!({"detail":refusal}))).into_response()
}

fn usable(text: &str) -> bool {
    let trimmed = text.trim();
    let compact = trimmed.chars().filter(|character| !character.is_whitespace()).collect::<String>();
    if trimmed.is_empty() || (!trimmed.chars().any(char::is_alphanumeric) && compact.chars().count() >= 8) {
        return false;
    }
    if compact.chars().count() >= 24 && compact.chars().collect::<std::collections::HashSet<_>>().len() <= 3 {
        return false;
    }
    let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
    if tokens.len() >= 10 && tokens.iter().copied().collect::<std::collections::HashSet<_>>().len() as f64 / tokens.len() as f64 < 0.15 {
        return false;
    }
    true
}

fn enough_speech(pcm: &[u8]) -> bool {
    let audible = pcm.chunks(640).map(|frame| {
        let rms = (frame.as_chunks::<2>().0.iter().map(|bytes| {
            let value = i16::from_le_bytes(*bytes) as f64 / 32768.0;
            value * value
        }).sum::<f64>() / (frame.len()/2) as f64).sqrt();
        if rms >= 0.008 { frame.len()/2 } else { 0 }
    }).sum::<usize>();
    audible >= 16_000 / 4
}

pub(super) async fn preview(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return fail("trial.origin_refused", StatusCode::FORBIDDEN, &headers);
    }
    if body.len() > MAX_BODY_BYTES {
        return fail("trial.audio_too_large", StatusCode::PAYLOAD_TOO_LARGE, &headers);
    }
    let Some(data) = payload(&body) else {
        return fail("trial.invalid_audio", StatusCode::BAD_REQUEST, &headers);
    };
    if data["place"] != "openai" {
        return fail("trial.invalid_stage", StatusCode::UNPROCESSABLE_ENTITY, &headers);
    }
    let options = data.get("options").cloned().unwrap_or_else(|| json!({}));
    let stage_input = json!({"place":"openai","model":data["model"],"options":options});
    let Some(stage) = crate::models::provider_check_stage("stt", &stage_input) else {
        return fail("trial.invalid_stage", StatusCode::UNPROCESSABLE_ENTITY, &headers);
    };
    let context = stage.options.get("context").and_then(Value::as_str).unwrap_or("").trim();
    if context.chars().count() > 400 {
        return fail("trial.invalid_stage", StatusCode::UNPROCESSABLE_ENTITY, &headers);
    }
    let audio = &data["audio"];
    if audio["encoding"] != "pcm_s16le" || audio["sample_rate"] != 16_000 {
        return fail("trial.invalid_audio", StatusCode::BAD_REQUEST, &headers);
    }
    let Some(encoded) = audio["data_base64"].as_str() else {
        return fail("trial.invalid_audio", StatusCode::BAD_REQUEST, &headers);
    };
    if encoded.len() > MAX_BODY_BYTES {
        return fail("trial.audio_too_large", StatusCode::PAYLOAD_TOO_LARGE, &headers);
    }
    let pcm = match base64::engine::general_purpose::STANDARD.decode(encoded) {
        Ok(pcm) => pcm,
        Err(_) => return fail("trial.invalid_audio", StatusCode::BAD_REQUEST, &headers),
    };
    if pcm.len() > MAX_PCM_BYTES {
        return fail("trial.audio_too_large", StatusCode::PAYLOAD_TOO_LARGE, &headers);
    }
    if pcm.len() < MIN_PCM_BYTES || !pcm.len().is_multiple_of(2) {
        return fail("trial.invalid_audio", StatusCode::BAD_REQUEST, &headers);
    }
    if !enough_speech(&pcm) {
        return fail("trial.silent", StatusCode::UNPROCESSABLE_ENTITY, &headers);
    }
    let Some(wav) = media::wav(&pcm, 16_000) else {
        return fail("trial.invalid_audio", StatusCode::BAD_REQUEST, &headers);
    };
    let Some(key) = media::provider_key(&state.dir, "openai") else {
        return fail("trial.provider_unavailable", StatusCode::CONFLICT, &headers);
    };
    let lease = match state.trial_budget.start(&device.0) {
        Ok(lease) => lease,
        Err(wait) => {
            let mut response = fail("trial.busy", StatusCode::TOO_MANY_REQUESTS, &headers);
            if let Ok(value) = HeaderValue::from_str(&wait.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            return response;
        }
    };
    let client = match OpenAiTranscriber::new(&key) {
        Ok(client) => client,
        Err(_) => return fail("trial.provider_unavailable", StatusCode::CONFLICT, &headers),
    };
    let language = stage.options.get("language").and_then(Value::as_str).filter(|value| *value != "auto");
    let response = tokio::time::timeout(Duration::from_secs(30), client.transcribe(
        &wav, &stage.model, language, (!context.is_empty()).then_some(context),
    )).await;
    drop(lease);
    match response {
        Err(_) => fail("trial.provider_timeout", StatusCode::GATEWAY_TIMEOUT, &headers),
        Ok(Err(error)) => match error.kind {
            ProviderErrorKind::Timeout => fail("trial.provider_timeout", StatusCode::GATEWAY_TIMEOUT, &headers),
            _ => fail("trial.provider_failed", StatusCode::BAD_GATEWAY, &headers),
        },
        Ok(Ok(result)) if usable(&result.text) => Json(json!({"text":result.text.trim()})).into_response(),
        Ok(Ok(result)) if result.text.trim().is_empty() => fail("trial.silent", StatusCode::UNPROCESSABLE_ENTITY, &headers),
        Ok(Ok(_)) => fail("trial.unusable", StatusCode::UNPROCESSABLE_ENTITY, &headers),
    }
}
