//! From an authenticated socket to a call: the hello, its settings, a room seat
//! and started media — or the refusal that ends the socket.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::messages::{render, render_refusal, LocalizedMessage};
use crate::pipeline::CallFrame;
use crate::server::media::{self, CallMedia};
use crate::server::AppState;
use crate::types::CallSettings;

use super::registration::CallRegistration;
use super::{close, text, UNPAIRED};

/// How long a call waits for the client's hello before it goes on with the default settings.
pub(super) const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Policy violation: the requested settings cannot run on this node.
const SETTINGS_REFUSED: u16 = 1008;
/// Try again later: the room has no seat for this call.
const ROOM_FULL: u16 = 1013;

/// What a call holds once the room and media accepted it.
pub(super) struct Admitted {
    /// The node's defaults, against which later settings are read.
    pub(super) defaults: CallSettings,
    pub(super) settings: CallSettings,
    pub(super) session: String,
    pub(super) media: Arc<CallMedia>,
    pub(super) detector_events: mpsc::Receiver<CallFrame>,
    pub(super) focus_events: mpsc::Receiver<()>,
    /// What the hello got wrong, as error events sent right after the session.
    pub(super) problems: Vec<Value>,
    /// What the room shows about this call's transcription.
    pub(super) transcription: Value,
}

/// Waits for the client's first text frame, answering pings meanwhile. No hello in time is a call with the default
/// settings.
pub(super) async fn await_hello(
    socket: &mut WebSocket,
    registration: &mut CallRegistration,
    close_reason: &str,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + HELLO_TIMEOUT;
    loop {
        tokio::select! {
            _ = registration.changed() => {
                let _ = socket.send(close(UNPAIRED, close_reason)).await;
                return None;
            }
            input = tokio::time::timeout_at(deadline, socket.recv()) => match input {
                Err(_) => return Some(json!({})),
                Ok(Some(Ok(Message::Text(raw)))) => {
                    return Some(serde_json::from_str::<Value>(&raw).unwrap_or_default());
                }
                Ok(Some(Ok(Message::Ping(bytes)))) => {
                    let _ = socket.send(Message::Pong(bytes)).await;
                }
                Ok(Some(Ok(Message::Pong(_)))) => {}
                _ => return None,
            }
        }
    }
}

/// Seats the call in the room and starts its media, or refuses it on the socket.
pub(super) async fn admit(
    state: &AppState,
    socket: &mut WebSocket,
    device: String,
    hello: &Value,
    events: mpsc::Sender<Value>,
) -> Option<Admitted> {
    let hello = hello.get("data");
    let (device_models, device_models_problem) =
        match crate::models::device_models(hello.and_then(|v| v.get("device_models"))) {
            Ok(report) => (report, None),
            Err(problem) => (None, Some(problem)),
        };
    let defaults = crate::models::default_settings(
        Some(&crate::runtime::system_language()),
        device_models.clone(),
    );
    let loaded = crate::models::settings_from(hello.and_then(|v| v.get("settings")), &defaults);
    // The detector runs on the room's numbers shaped by the device's patience; the device's own
    // tuning, sent or stored, never reaches it (the 2026-09-20 regression).
    let (mic, mic_problem) =
        crate::models::mic_settings(&loaded.settings, hello.and_then(|v| v.get("mic")));
    let settings = mic.applied_to(&loaded.settings);
    if let Some(refusal) = unavailable_refusal(state, &settings) {
        let _ = socket.send(text(&refusal)).await;
        let _ = socket.send(close(SETTINGS_REFUSED, "")).await;
        return None;
    }
    let Ok(session) = state
        .room
        .join(device, settings.ui_language.clone(), events)
    else {
        let admission = state.room.admission(&settings.ui_language);
        refuse_full(socket, &admission).await;
        return None;
    };
    let Ok((media, detector_events, focus_events)) = CallMedia::start(&settings) else {
        state.room.leave(&session);
        let message = render(
            &LocalizedMessage::new("voice.media_unavailable"),
            &settings.ui_language,
        );
        let refusal =
            json!({"type":"error","data":{"key":"voice.media_unavailable","message":message}});
        let _ = socket.send(text(&refusal)).await;
        return None;
    };
    let language = settings.ui_language.as_str();
    let settings_problem = loaded
        .issue
        .map(|message| json!({"key":"settings.invalid","message":message}))
        .or_else(|| {
            mic_problem.map(|message| json!({"key":"turn_patience_unknown","message":message}))
        });
    let (runtime, runtime_problem) = match crate::models::browser_runtime(
        hello.and_then(|v| v.get("transcription")),
        device_models.as_ref(),
    ) {
        Ok(runtime) => (runtime, None),
        Err(problem) => (None, Some(runtime_refusal(&problem, language))),
    };
    let device_models_problem =
        device_models_problem.map(|problem| runtime_refusal(&problem, language));
    let problems = [device_models_problem, settings_problem, runtime_problem]
        .into_iter()
        .flatten()
        .map(|problem| json!({"type":"error","data":problem}))
        .collect();
    let transcription = crate::models::call_transcription(&settings.stt, runtime.as_ref());
    state
        .room
        .set_transcription(&session, transcription.clone());
    Some(Admitted {
        defaults,
        settings,
        session,
        media,
        detector_events,
        focus_events,
        problems,
        transcription,
    })
}

/// Refuses a client the room has no seat for: the reason as a frame, then 1013 (try again later).
pub(super) async fn refuse_full(socket: &mut WebSocket, admission: &Value) {
    let refusal = json!({"type":"error","data":{"message":admission["message"],"reason":admission["reason"]}});
    let _ = socket.send(text(&refusal)).await;
    let _ = socket.send(close(ROOM_FULL, "")).await;
}

/// The error event for settings whose provider this node cannot reach, if any.
pub(super) fn unavailable_refusal(state: &AppState, settings: &CallSettings) -> Option<Value> {
    let refusal = crate::models::unavailable(settings, |place| {
        media::provider_key(&state.dir, place).is_some()
    })?;
    Some(json!({"type":"error","data":render_refusal(&refusal, &settings.ui_language)}))
}

/// The error data for a transcription runtime or a device-models report this node cannot read.
pub(super) fn runtime_refusal(problem: &LocalizedMessage, language: &str) -> Value {
    json!({"key":problem.key,"message":render(problem, language)})
}
