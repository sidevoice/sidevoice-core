//! What a device may choose from: default settings, the model catalogue, and the
//! providers' transcription models and voices.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::messages::{render, LocalizedMessage};
use crate::providers::{ElevenLabsCatalog, ElevenLabsTts, OpenAiTranscriber};
use crate::server::media;
use crate::server::refusal::{refuse, require_origin, Handled};
use crate::server::request::{accept_language, query};
use crate::server::AppState;

use super::provider_errors::catalog_error;

pub(super) async fn languages() -> Json<Value> {
    // The defaults are sent with null builds. Settings still
    // belong to the device; this route only supplies the catalogue defaults.
    let mut settings = serde_json::to_value(crate::models::default_settings(None, None))
        .expect("default settings serialize");
    for stage in ["stt", "tts"] {
        settings[stage]["build"] = Value::Null;
    }
    // CallSettings uses f32 for detector input; the response gives the decimal
    // defaults rather than their f32 runtime representation.
    settings["smart_turn_min_silence"] = json!(0.9);
    settings["vad_confidence"] = json!(0.6);
    settings["vad_start_secs"] = json!(0.4);
    Json(settings)
}

pub(super) async fn model_catalog(headers: HeaderMap) -> Handled {
    require_origin(&headers)?;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        crate::models::catalog_text(),
    )
        .into_response())
}

pub(super) async fn transcription_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Handled {
    require_origin(&headers)?;
    if query(&uri, "provider").as_deref() != Some("openai") {
        return Err(refuse(
            "integration.unknown",
            StatusCode::BAD_REQUEST,
            &headers,
        ));
    }
    let Some(key) = media::provider_key(&state.dir, "openai") else {
        return Ok(
            Json(json!({"provider":"openai","configured":false,"models":[],"error":null}))
                .into_response(),
        );
    };
    let models = match OpenAiTranscriber::new(&key) {
        Ok(client) => client.catalog().await,
        Err(error) => Err(error),
    };
    let listing = match models {
        Ok(ids) => {
            let empty = ids.is_empty();
            json!({"provider":"openai","configured":true,
                "models":ids.into_iter().map(|id|json!({"label":id,"id":id})).collect::<Vec<_>>(),
                "error":if empty {Some(render(&LocalizedMessage::new("integration.catalog_empty"),accept_language(&headers)))} else {None}})
        }
        Err(error) => {
            json!({"provider":"openai","configured":true,"models":[],"error":catalog_error(&error,&headers)})
        }
    };
    Ok(Json(listing).into_response())
}

pub(super) async fn voice_catalog(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Handled {
    require_origin(&headers)?;
    let language = accept_language(&headers);
    let key = media::provider_key(&state.dir, "elevenlabs");
    let configured = key.is_some();
    let catalog = match key {
        Some(key) => match ElevenLabsTts::new(&key) {
            Ok(client) => client.catalog(language).await,
            Err(error) => ElevenLabsCatalog {
                error: Some(error),
                ..ElevenLabsCatalog::unconfigured(language)
            },
        },
        None => ElevenLabsCatalog::unconfigured(language),
    };
    let models = catalog
        .models
        .into_iter()
        .map(|model| {
            let mut entry = json!({"id":model.id,"label":model.label});
            if let Some(description) = model.description {
                entry["description"] = json!(description);
            }
            entry
        })
        .collect::<Vec<_>>();
    let voices = catalog
        .voices
        .into_iter()
        .map(|voice| {
            json!({"id":voice.id,"label":voice.label,"description":voice.description,
            "languages":voice.languages})
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"languages":crate::models::voice_languages(),
        "providers":{"elevenlabs":{"configured":configured,"models":models,"voices":voices,
            "error":catalog.error.as_ref().map(|error|catalog_error(error,&headers))}}}))
    .into_response())
}
