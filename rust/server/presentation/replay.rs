//! Replaying an assistant reply from the synthesis cache, pinned until it plays.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::models::ResolvedVoice;
use crate::providers::cache::{SynthesisCache, SynthesisChoice};
use crate::providers::CloudSpeech;
use crate::server::refusal::{refuse, require_origin, room_refusal, Handled};
use crate::server::request::room_payload;
use crate::server::{AppState, AuthenticatedDevice, PinnedReplay};
use crate::types::CallSettings;

/// The cached speech of `text` in the voice the call's settings resolve to.
fn cached_reply(
    state: &AppState,
    settings: &CallSettings,
    text: &str,
    language: Option<&str>,
) -> Option<(Arc<CloudSpeech>, ResolvedVoice)> {
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

/// Flags the assistant rows of `history` whose audio this call could replay.
pub(super) fn mark_replayable(state: &AppState, sid: &str, history: &mut Value) {
    let settings = state
        .call_settings
        .lock()
        .expect("call settings lock")
        .get(sid)
        .cloned();
    let (Some(settings), Some(rows)) = (settings, history["messages"].as_array_mut()) else {
        return;
    };
    for row in rows {
        if row["role"] != "assistant" {
            continue;
        }
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        let Ok((text, language)) = state.room.replay_source(sid, id) else {
            continue;
        };
        if cached_reply(state, &settings, &text, language.as_deref()).is_some() {
            row["replayable"] = json!(true);
        }
    }
}

pub(super) async fn replay(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    body: Bytes,
) -> Handled {
    require_origin(&headers)?;
    let data = room_payload(&body, &headers)?;
    let sid = data["session_id"].as_str().unwrap_or("");
    let history_id = data["history_id"].as_str().unwrap_or("");
    let absent = || refuse("room.browser_absent", StatusCode::CONFLICT, &headers);
    if !state.room.owns_session(sid, &device.0) {
        return Err(absent());
    }
    if history_id.is_empty() {
        return Err(refuse(
            "room.replay_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        ));
    }
    let (text, language) = state
        .room
        .replay_source(sid, history_id)
        .map_err(|error| room_refusal(error, &headers))?;
    let settings = state
        .call_settings
        .lock()
        .expect("call settings lock")
        .get(sid)
        .cloned();
    let settings = settings.ok_or_else(absent)?;
    let Some((speech, voice)) = cached_reply(&state, &settings, &text, language.as_deref()) else {
        return Err(refuse(
            "room.replay_audio_missing",
            StatusCode::GONE,
            &headers,
        ));
    };
    let uid = format!("{sid}:replay:{}", Uuid::new_v4());
    // Holding the audio lock across admission keeps it atomic with session teardown.
    let mut pending = state.replay_audio.lock().expect("replay audio lock");
    pending.insert(uid.clone(), Arc::new(PinnedReplay { speech, voice }));
    match state.room.replay_one(sid, history_id, &uid) {
        Ok(value) => Ok(Json(value).into_response()),
        Err(error) => {
            pending.remove(&uid);
            Err(room_refusal(error, &headers))
        }
    }
}
