//! Provider keys saved on this node: listing, verifying and saving, and clearing.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::providers::{verify_elevenlabs_key, verify_openai_key};
use crate::server::refusal::{refuse, require_origin, Handled};
use crate::server::request::payload;
use crate::server::trust::origin_allowed;
use crate::server::AppState;

use super::provider_errors::provider_refusal;

const STORE: &str = "integrations.json";
const PROVIDERS: [&str; 2] = ["openai", "elevenlabs"];

struct ProviderMeta {
    label: &'static str,
    capability: &'static str,
    environment: &'static str,
}

fn provider_meta(provider: &str) -> Option<ProviderMeta> {
    let (label, capability, environment) = match provider {
        "openai" => ("OpenAI", "transcription", "VOICE_STT_API_KEY"),
        "elevenlabs" => ("ElevenLabs", "voice", "VOICE_ELEVENLABS_API_KEY"),
        _ => return None,
    };
    Some(ProviderMeta {
        label,
        capability,
        environment,
    })
}

fn integration_listing(state: &AppState) -> Value {
    let saved = state.dir.read_json(STORE).ok().flatten();
    let providers = PROVIDERS
        .into_iter()
        .map(|id| {
            let meta = provider_meta(id).expect("known provider");
            let stored = saved.as_ref().and_then(|value| value[id].as_str());
            let deployed = std::env::var(meta.environment).ok();
            let status = crate::models::credential_state(stored, deployed.as_deref());
            json!({"id":id,"label":meta.label,"capabilities":[meta.capability],
                "configured":status.configured,"source":status.source,
                "hint":status.hint,"environment":meta.environment})
        })
        .collect::<Vec<_>>();
    json!({"providers":providers})
}

pub(super) async fn listing(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Handled {
    require_origin(&headers)?;
    Ok(Json(integration_listing(&state)).into_response())
}

/// Writes need a known provider and a trusted browser origin, not merely an allowed one.
fn write_allowed(headers: &HeaderMap, provider: &str) -> Result<(), (&'static str, StatusCode)> {
    if provider_meta(provider).is_none() {
        return Err(("integration.unknown", StatusCode::NOT_FOUND));
    }
    if headers.get(header::ORIGIN).is_none() || !origin_allowed(headers) {
        return Err(("request.origin_invalid", StatusCode::FORBIDDEN));
    }
    Ok(())
}

/// Starts a new write for `provider`, superseding any verification in flight.
fn next_revision(revisions: &mut HashMap<String, u64>, provider: &str) -> u64 {
    let revision = revisions.entry(provider.to_owned()).or_default();
    *revision = revision.wrapping_add(1);
    *revision
}

fn saved_integrations(state: &AppState) -> Value {
    state
        .dir
        .read_json(STORE)
        .ok()
        .flatten()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

pub(super) async fn save(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    write_allowed(&headers, &provider).map_err(|(key, status)| refuse(key, status, &headers))?;
    let Some(key) = payload(&body)
        .and_then(|body| body["key"].as_str().map(str::trim).map(str::to_owned))
        .filter(|key| !key.is_empty())
    else {
        return Err(refuse(
            "integration.key_empty",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        ));
    };
    let ticket = next_revision(
        &mut state
            .integration_revisions
            .lock()
            .expect("integration revisions lock"),
        &provider,
    );
    let verified = match provider.as_str() {
        "openai" => verify_openai_key(&key).await,
        "elevenlabs" => verify_elevenlabs_key(&key).await,
        _ => unreachable!("provider checked above"),
    };
    verified.map_err(|error| provider_refusal(&error, &headers))?;
    let revisions = state
        .integration_revisions
        .lock()
        .expect("integration revisions lock");
    if revisions.get(&provider) != Some(&ticket) {
        // Keyed like the Python core's refusal, which the page translates by this key.
        let refusal = crate::messages::render_refusal(
            &crate::messages::LocalizedMessage::new("integration_superseded"),
            crate::server::request::accept_language(&headers),
        );
        return Err((StatusCode::CONFLICT, Json(json!({ "detail": refusal })))
            .into_response()
            .into());
    }
    let mut saved = saved_integrations(&state);
    saved[&provider] = json!(key);
    if state.dir.write_json(STORE, &saved).is_err() {
        return Err(refuse(
            "start.failed",
            StatusCode::INTERNAL_SERVER_ERROR,
            &headers,
        ));
    }
    drop(revisions);
    Ok(Json(integration_listing(&state)).into_response())
}

pub(super) async fn clear(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> Handled {
    write_allowed(&headers, &provider).map_err(|(key, status)| refuse(key, status, &headers))?;
    let mut revisions = state
        .integration_revisions
        .lock()
        .expect("integration revisions lock");
    next_revision(&mut revisions, &provider);
    let mut saved = saved_integrations(&state);
    if saved
        .as_object_mut()
        .expect("object")
        .remove(&provider)
        .is_some()
        && state.dir.write_json(STORE, &saved).is_err()
    {
        return Err(refuse(
            "start.failed",
            StatusCode::INTERNAL_SERVER_ERROR,
            &headers,
        ));
    }
    drop(revisions);
    Ok(Json(integration_listing(&state)).into_response())
}
