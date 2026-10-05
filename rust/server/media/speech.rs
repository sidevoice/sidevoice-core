//! Prepares one `voice-speech` event for the browser: device speech passes through
//! with its voice, cloud speech is synthesized (or replayed) and attached as audio.

use std::sync::Arc;

use base64::Engine;
use serde_json::{json, Map, Value};

use super::keys::provider_key;
use crate::{
    control::room::{latency_now_micros, LatencyEvent, Room},
    models::{resolve_voice, ResolvedVoice},
    providers::{
        cache::{CachedSpeech, SynthesisCache, SynthesisChoice},
        ElevenLabsTts,
    },
    server::PinnedReplay,
    storage::PrivateDir,
    types::CallSettings,
};

/// Provider timings forwarded to the room's latency record for fresh renders.
const PROVIDER_TIMINGS: [&str; 3] = [
    "request_to_headers_ms",
    "request_to_first_chunk_ms",
    "request_to_complete_ms",
];

/// Returns the event to send, or `None` when it must not be sent at all.
pub(in crate::server) async fn speech_event(
    room: Arc<Room>,
    session: &str,
    settings: &CallSettings,
    dir: &PrivateDir,
    cache: Arc<SynthesisCache>,
    event: Value,
    replay_audio: Option<Arc<PinnedReplay>>,
) -> Option<Value> {
    if event.get("type").and_then(Value::as_str) != Some("voice-speech") {
        return Some(event);
    }
    let data = &event["data"];
    let uid = data["utterance_id"].as_str()?;
    let replay = uid.starts_with(&format!("{session}:replay:"));
    if replay && replay_audio.is_none() {
        // A replay owns only its admitted bytes. Never synthesize a missing pin.
        return None;
    }
    #[cfg(feature = "hosted-fixtures")]
    if replay {
        wait_at_fixture_gate(uid).await;
    }
    let revision = data["revision"].as_u64()?;
    let reply_revision = data["reply_revision"].as_u64().unwrap_or(revision);
    let thread = data["thread_id"].as_str()?;
    let text = data["text"].as_str()?;
    let voice = match &replay_audio {
        Some(pin) => pin.voice.clone(),
        None => resolve_voice(settings, data["language"].as_str()).ok()?,
    };
    let mut message = data.clone();
    let object = message.as_object_mut()?;
    describe_voice(object, &voice);
    let mark = |event| {
        room.latency_mark(
            session,
            thread,
            reply_revision,
            Some(uid),
            event,
            latency_now_micros(),
        )
    };
    if voice.place == "device" {
        if !room.speech_current(session, uid, revision) {
            return None;
        }
        mark(LatencyEvent::AudioDispatched);
        return Some(json!({"type":"voice-speech","data":message}));
    }
    let result = match replay_audio {
        Some(pin) => CachedSpeech {
            speech: pin.speech.clone(),
            fresh: false,
        },
        None => synthesize(dir, &cache, &voice, text).await?,
    };
    mark(LatencyEvent::AudioReady);
    if !room.speech_current(session, uid, revision) {
        return None;
    }
    attach_audio(object, &result);
    if result.fresh {
        for name in PROVIDER_TIMINGS {
            if let Some(value) = result.speech.timings_ms.get(name).and_then(Value::as_f64) {
                room.latency_duration(session, thread, reply_revision, Some(uid), name, value);
            }
        }
    }
    mark(LatencyEvent::AudioDispatched);
    Some(json!({"type":"voice-speech-audio","data":message}))
}

pub(super) fn describe_voice(object: &mut Map<String, Value>, voice: &ResolvedVoice) {
    object.insert("place".into(), json!(&voice.place));
    object.insert("model".into(), json!(&voice.model));
    object.insert("voice".into(), json!(&voice.voice));
    object.insert("speed".into(), json!(voice.speed));
    object.insert("language".into(), json!(&voice.language));
}

/// Shared renders carry no timings: they belong to the request that rendered them.
pub(super) fn attach_audio(object: &mut Map<String, Value>, result: &CachedSpeech) {
    object.insert("mime_type".into(), json!(&result.speech.mime_type));
    object.insert(
        "audio_base64".into(),
        json!(base64::engine::general_purpose::STANDARD.encode(&result.speech.audio)),
    );
    object.insert("alignment".into(), json!(result.speech.alignment));
    object.insert(
        "timings_ms".into(),
        json!(if result.fresh {
            result.speech.timings_ms.clone()
        } else {
            Map::new()
        }),
    );
    object.insert("shared".into(), json!(!result.fresh));
}

async fn synthesize(
    dir: &PrivateDir,
    cache: &SynthesisCache,
    voice: &ResolvedVoice,
    text: &str,
) -> Option<CachedSpeech> {
    let key = provider_key(dir, "elevenlabs")?;
    let client = ElevenLabsTts::new(&key).ok()?;
    let choice = SynthesisChoice {
        place: &voice.place,
        model: &voice.model,
        voice: &voice.voice,
        speed: voice.speed,
    };
    let model = voice.model.clone();
    let voice_id = voice.voice.clone();
    let speed = voice.speed;
    let text_owned = text.to_owned();
    cache
        .obtain(choice, text, move || async move {
            client
                .synthesize(&text_owned, &model, &voice_id, speed, true, "mp3_44100_128")
                .await
        })
        .await
        .ok()
}

/// Hosted fixtures pause a replay render while the gate file exists.
#[cfg(feature = "hosted-fixtures")]
async fn wait_at_fixture_gate(uid: &str) {
    if let Ok(gate) = std::env::var("SIDEVOICE_FIXTURE_REPLAY_RENDER_GATE") {
        let path = std::path::Path::new(&gate);
        if path.exists() {
            let _ = std::fs::write(format!("{gate}.entered"), uid);
            while path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
}
