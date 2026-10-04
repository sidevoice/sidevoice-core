//! Settings and try routes backed by the node's existing provider and private stores.

use super::*;
use crate::providers::cache::{SynthesisCache, SynthesisChoice};
use crate::providers::{
    verify_elevenlabs_key, verify_openai_key, ElevenLabsCatalog, ElevenLabsTts, OpenAiTranscriber,
    ProviderError, ProviderErrorKind,
};
use axum::body::Bytes;

pub(super) fn cached_reply(
    state: &AppState,
    settings: &crate::types::CallSettings,
    text: &str,
    language: Option<&str>,
) -> Option<(
    Arc<crate::providers::CloudSpeech>,
    crate::models::ResolvedVoice,
)> {
    let voice = crate::models::resolve_voice(settings, language).ok()?;
    if voice.place == "device" {
        return None;
    }
    let key = SynthesisCache::key(
        SynthesisChoice {
            place: &voice.place,
            model: &voice.model,
            voice: &voice.voice,
            speed: voice.speed,
        },
        text,
    );
    state.synthesis.read(&key).map(|speech| (speech, voice))
}

fn provider_meta(provider: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match provider {
        "openai" => Some(("OpenAI", "transcription", "VOICE_STT_API_KEY")),
        "elevenlabs" => Some(("ElevenLabs", "voice", "VOICE_ELEVENLABS_API_KEY")),
        _ => None,
    }
}

pub(super) fn integration_listing(state: &AppState) -> Value {
    let saved = state.dir.read_json("integrations.json").ok().flatten();
    let providers = ["openai", "elevenlabs"]
        .into_iter()
        .map(|id| {
            let (label, capability, environment) = provider_meta(id).expect("known provider");
            let stored = saved.as_ref().and_then(|value| value[id].as_str());
            let deployed = std::env::var(environment).ok();
            let status = crate::models::credential_state(stored, deployed.as_deref());
            json!({"id":id,"label":label,"capabilities":[capability],
                "configured":status.configured,"source":status.source,
                "hint":status.hint,"environment":environment})
        })
        .collect::<Vec<_>>();
    json!({"providers":providers})
}

fn write_allowed(headers: &HeaderMap, provider: &str) -> Result<(), (&'static str, StatusCode)> {
    if provider_meta(provider).is_none() {
        return Err(("integration.unknown", StatusCode::NOT_FOUND));
    }
    if headers.get(header::ORIGIN).is_none() || !origin_allowed(headers) {
        return Err(("request.origin_invalid", StatusCode::FORBIDDEN));
    }
    Ok(())
}

fn provider_failure(error: &ProviderError, headers: &HeaderMap) -> Response {
    let key = match error.kind {
        ProviderErrorKind::Unauthorized => "integration.key_refused",
        ProviderErrorKind::Timeout | ProviderErrorKind::Transport => "integration.unreachable",
        _ => "integration.verify_failed",
    };
    failure(key, StatusCode::UNPROCESSABLE_ENTITY, headers)
}

pub(super) async fn save_integration(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((key, status)) = write_allowed(&headers, &provider) {
        return failure(key, status, &headers);
    }
    let Some(key) = payload(&body)
        .and_then(|body| body["key"].as_str().map(str::trim).map(str::to_owned))
        .filter(|key| !key.is_empty())
    else {
        return failure(
            "integration.key_empty",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        );
    };
    let ticket = {
        let mut revisions = state
            .integration_revisions
            .lock()
            .expect("integration revisions lock");
        let revision = revisions.entry(provider.clone()).or_default();
        *revision = revision.wrapping_add(1);
        *revision
    };
    let verified = match provider.as_str() {
        "openai" => verify_openai_key(&key).await,
        "elevenlabs" => verify_elevenlabs_key(&key).await,
        _ => unreachable!("provider checked above"),
    };
    if let Err(error) = verified {
        return provider_failure(&error, &headers);
    }
    let revisions = state
        .integration_revisions
        .lock()
        .expect("integration revisions lock");
    if revisions.get(&provider) != Some(&ticket) {
        return failure("integration.superseded", StatusCode::CONFLICT, &headers);
    }
    let mut saved = state
        .dir
        .read_json("integrations.json")
        .ok()
        .flatten()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    saved[&provider] = json!(key);
    if state.dir.write_json("integrations.json", &saved).is_err() {
        return failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers);
    }
    drop(revisions);
    Json(integration_listing(&state)).into_response()
}

pub(super) async fn clear_integration(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err((key, status)) = write_allowed(&headers, &provider) {
        return failure(key, status, &headers);
    }
    let mut revisions = state
        .integration_revisions
        .lock()
        .expect("integration revisions lock");
    let revision = revisions.entry(provider.clone()).or_default();
    *revision = revision.wrapping_add(1);
    let mut saved = state
        .dir
        .read_json("integrations.json")
        .ok()
        .flatten()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if saved
        .as_object_mut()
        .expect("object")
        .remove(&provider)
        .is_some()
        && state.dir.write_json("integrations.json", &saved).is_err()
    {
        return failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers);
    }
    drop(revisions);
    Json(integration_listing(&state)).into_response()
}

fn language(headers: &HeaderMap) -> &str {
    headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .unwrap_or("en")
}

fn catalog_error(error: &ProviderError, headers: &HeaderMap) -> String {
    let key = match error.kind {
        ProviderErrorKind::Unauthorized => "integration.key_refused",
        ProviderErrorKind::Timeout | ProviderErrorKind::Transport => "integration.unreachable",
        _ => "integration.catalog_failed",
    };
    render(&LocalizedMessage::new(key), language(headers))
}

pub(super) async fn transcription_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    if query(&uri, "provider").as_deref() != Some("openai") {
        return failure("integration.unknown", StatusCode::BAD_REQUEST, &headers);
    }
    let Some(key) = media::provider_key(&state.dir, "openai") else {
        return Json(json!({"provider":"openai","configured":false,"models":[],"error":null}))
            .into_response();
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
                "error":if empty {Some(render(&LocalizedMessage::new("integration.catalog_empty"),language(&headers)))} else {None}})
        }
        Err(error) => {
            json!({"provider":"openai","configured":true,"models":[],"error":catalog_error(&error,&headers)})
        }
    };
    Json(listing).into_response()
}

pub(super) async fn voice_catalog(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let key = media::provider_key(&state.dir, "elevenlabs");
    let configured = key.is_some();
    let catalog = match key {
        Some(key) => match ElevenLabsTts::new(&key) {
            Ok(client) => client.catalog(language(&headers)).await,
            Err(error) => ElevenLabsCatalog {
                error: Some(error),
                ..ElevenLabsCatalog::unconfigured(language(&headers))
            },
        },
        None => ElevenLabsCatalog::unconfigured(language(&headers)),
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
    Json(json!({"languages":crate::models::voice_languages(),
        "providers":{"elevenlabs":{"configured":configured,"models":models,"voices":voices,
            "error":catalog.error.as_ref().map(|error|catalog_error(error,&headers))}}}))
    .into_response()
}

pub(super) async fn synthesis_preview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(body) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    let (Some(key), Some(model), Some(voice)) = (
        media::provider_key(&state.dir, "elevenlabs"),
        body["model"].as_str().filter(|value| !value.is_empty()),
        body["voice"].as_str().filter(|value| !value.is_empty()),
    ) else {
        return failure(
            "integration.preview_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        );
    };
    let speed = body["speed"].as_f64().unwrap_or(1.0);
    let text = body["text"].as_str().unwrap_or("");
    let client = match ElevenLabsTts::new(&key) {
        Ok(client) => client,
        Err(error) => return provider_failure(&error, &headers),
    };
    match client
        .synthesize(text, model, voice, speed, false, "mp3_44100_128")
        .await
    {
        Ok(speech) => Json(json!({"mime_type":speech.mime_type,
            "audio_base64":base64::engine::general_purpose::STANDARD.encode(speech.audio),
            "timings_ms":speech.timings_ms,"alignment":speech.alignment}))
        .into_response(),
        Err(error) => provider_failure(&error, &headers),
    }
}

pub(super) async fn replay(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    let sid = data["session_id"].as_str().unwrap_or("");
    let history_id = data["history_id"].as_str().unwrap_or("");
    if !state.room.owns_session(sid, &device.0) {
        return failure("room.browser_absent", StatusCode::CONFLICT, &headers);
    }
    if history_id.is_empty() {
        return failure(
            "room.replay_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        );
    }
    let (text, language) = match state.room.replay_source(sid, history_id) {
        Ok(source) => source,
        Err(error) => return room_failure(error, &headers),
    };
    let settings = state
        .call_settings
        .lock()
        .expect("call settings lock")
        .get(sid)
        .cloned();
    let Some(settings) = settings else {
        return failure("room.browser_absent", StatusCode::CONFLICT, &headers);
    };
    let Some((speech, voice)) = cached_reply(&state, &settings, &text, language.as_deref()) else {
        return failure("room.replay_audio_missing", StatusCode::GONE, &headers);
    };
    let uid = format!("{sid}:replay:{}", Uuid::new_v4());
    let mut pending = state.replay_audio.lock().expect("replay audio lock");
    pending.insert(uid.clone(), Arc::new(PinnedReplay { speech, voice }));
    match state.room.replay_one(sid, history_id, &uid) {
        Ok(value) => Json(value).into_response(),
        Err(error) => {
            pending.remove(&uid);
            room_failure(error, &headers)
        }
    }
}
