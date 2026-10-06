//! One bounded, non-room transcription of a selected device's spoken sample.

use super::*;
use crate::messages::LocalizedMessage;
use crate::providers::{OpenAiTranscriber, ProviderError, ProviderErrorKind, Transcription};
use crate::types::SpeechStage;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::time::Duration;

mod budget;
mod speech;
#[cfg(test)]
mod tests;

pub(super) use budget::TrialBudget;

use speech::{enough_speech, usable};

const MAX_BODY_BYTES: usize = 512 * 1024;
const MAX_PCM_BYTES: usize = 10 * 16_000 * 2;
const MIN_PCM_BYTES: usize = 16_000 / 4 * 2;
const MAX_CONTEXT_CHARS: usize = 400;
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);

fn fail(key: &str, status: StatusCode, headers: &HeaderMap) -> Response {
    let language = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("en");
    let refusal = crate::messages::render_refusal(&LocalizedMessage::new(key), language);
    (status, Json(json!({"detail":refusal}))).into_response()
}

/// A refusal's message key and status, rendered by `fail` once the request's language is known.
type Refused = (&'static str, StatusCode);

const INVALID_STAGE: Refused = ("trial.invalid_stage", StatusCode::UNPROCESSABLE_ENTITY);
const INVALID_AUDIO: Refused = ("trial.invalid_audio", StatusCode::BAD_REQUEST);
const AUDIO_TOO_LARGE: Refused = ("trial.audio_too_large", StatusCode::PAYLOAD_TOO_LARGE);

/// The OpenAI transcription stage the trial asks for, refused unless the call's catalogue rules accept it.
fn trial_stage(data: &Value) -> Result<SpeechStage, Refused> {
    if data["place"] != "openai" {
        return Err(INVALID_STAGE);
    }
    let options = data.get("options").cloned().unwrap_or_else(|| json!({}));
    let stage_input = json!({"place":"openai","model":data["model"],"options":options});
    let stage = crate::models::provider_check_stage("stt", &stage_input).ok_or(INVALID_STAGE)?;
    if context(&stage).chars().count() > MAX_CONTEXT_CHARS {
        return Err(INVALID_STAGE);
    }
    Ok(stage)
}

fn context(stage: &SpeechStage) -> &str {
    stage
        .options
        .get("context")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

/// The sample's 16 kHz 16-bit PCM, refused when it is malformed, too large, too short or silent.
fn trial_pcm(data: &Value) -> Result<Vec<u8>, Refused> {
    let audio = &data["audio"];
    if audio["encoding"] != "pcm_s16le" || audio["sample_rate"] != 16_000 {
        return Err(INVALID_AUDIO);
    }
    let Some(encoded) = audio["data_base64"].as_str() else {
        return Err(INVALID_AUDIO);
    };
    if encoded.len() > MAX_BODY_BYTES {
        return Err(AUDIO_TOO_LARGE);
    }
    let pcm = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| INVALID_AUDIO)?;
    if pcm.len() > MAX_PCM_BYTES {
        return Err(AUDIO_TOO_LARGE);
    }
    if pcm.len() < MIN_PCM_BYTES || !pcm.len().is_multiple_of(2) {
        return Err(INVALID_AUDIO);
    }
    if !enough_speech(&pcm) {
        return Err(("trial.silent", StatusCode::UNPROCESSABLE_ENTITY));
    }
    Ok(pcm)
}

fn busy(wait: u64, headers: &HeaderMap) -> Response {
    let mut response = fail("trial.busy", StatusCode::TOO_MANY_REQUESTS, headers);
    if let Ok(value) = HeaderValue::from_str(&wait.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// The transcript, or the refusal for a provider that timed out, failed or heard nothing usable.
fn transcription_response(
    response: Result<Result<Transcription, ProviderError>, tokio::time::error::Elapsed>,
    headers: &HeaderMap,
) -> Response {
    match response {
        Err(_) => fail(
            "trial.provider_timeout",
            StatusCode::GATEWAY_TIMEOUT,
            headers,
        ),
        Ok(Err(error)) => match error.kind {
            ProviderErrorKind::Timeout => fail(
                "trial.provider_timeout",
                StatusCode::GATEWAY_TIMEOUT,
                headers,
            ),
            _ => fail("trial.provider_failed", StatusCode::BAD_GATEWAY, headers),
        },
        Ok(Ok(result)) if usable(&result.text) => {
            Json(json!({"text":result.text.trim()})).into_response()
        }
        Ok(Ok(result)) if result.text.trim().is_empty() => {
            fail("trial.silent", StatusCode::UNPROCESSABLE_ENTITY, headers)
        }
        Ok(Ok(_)) => fail("trial.unusable", StatusCode::UNPROCESSABLE_ENTITY, headers),
    }
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
        return fail(
            "trial.audio_too_large",
            StatusCode::PAYLOAD_TOO_LARGE,
            &headers,
        );
    }
    let Some(data) = payload(&body) else {
        return fail("trial.invalid_audio", StatusCode::BAD_REQUEST, &headers);
    };
    let trial = trial_stage(&data).and_then(|stage| trial_pcm(&data).map(|pcm| (stage, pcm)));
    let (stage, pcm) = match trial {
        Ok(trial) => trial,
        Err((key, status)) => return fail(key, status, &headers),
    };
    let Some(wav) = media::wav(&pcm, 16_000) else {
        return fail("trial.invalid_audio", StatusCode::BAD_REQUEST, &headers);
    };
    let Some(key) = media::provider_key(&state.dir, "openai") else {
        return fail("trial.provider_unavailable", StatusCode::CONFLICT, &headers);
    };
    let lease = match state.trial_budget.start(&device.0) {
        Ok(lease) => lease,
        Err(wait) => return busy(wait, &headers),
    };
    let client = match OpenAiTranscriber::new(&key) {
        Ok(client) => client,
        Err(_) => return fail("trial.provider_unavailable", StatusCode::CONFLICT, &headers),
    };
    let language = stage
        .options
        .get("language")
        .and_then(Value::as_str)
        .filter(|value| *value != "auto");
    let context = context(&stage);
    let response = tokio::time::timeout(
        PROVIDER_TIMEOUT,
        client.transcribe(
            &wav,
            &stage.model,
            language,
            (!context.is_empty()).then_some(context),
        ),
    )
    .await;
    drop(lease);
    transcription_response(response, &headers)
}
