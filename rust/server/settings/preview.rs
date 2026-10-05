//! Trying a voice before choosing it: one synthesis with the saved ElevenLabs key.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use base64::Engine;
use serde_json::json;

use crate::providers::ElevenLabsTts;
use crate::server::media;
use crate::server::refusal::{refuse, require_origin, Handled};
use crate::server::request::room_payload;
use crate::server::AppState;

use super::provider_errors::provider_refusal;

const PREVIEW_FORMAT: &str = "mp3_44100_128";

pub(super) async fn synthesis_preview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let body = room_payload(&body, &headers)?;
    let (Some(key), Some(model), Some(voice)) = (
        media::provider_key(&state.dir, "elevenlabs"),
        body["model"].as_str().filter(|value| !value.is_empty()),
        body["voice"].as_str().filter(|value| !value.is_empty()),
    ) else {
        return Err(refuse(
            "integration.preview_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        ));
    };
    let speed = body["speed"].as_f64().unwrap_or(1.0);
    let text = body["text"].as_str().unwrap_or("");
    let client = ElevenLabsTts::new(&key).map_err(|error| provider_refusal(&error, &headers))?;
    let speech = client
        .synthesize(text, model, voice, speed, false, PREVIEW_FORMAT)
        .await
        .map_err(|error| provider_refusal(&error, &headers))?;
    Ok(Json(json!({"mime_type":speech.mime_type,
        "audio_base64":base64::engine::general_purpose::STANDARD.encode(speech.audio),
        "timings_ms":speech.timings_ms,"alignment":speech.alignment}))
    .into_response())
}
