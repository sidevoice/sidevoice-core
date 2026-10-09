//! From an authenticated socket to a call: the hello, its interface language and a room seat, or the
//! refusal that ends the socket.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::messages::{render, ui_locale, LocalizedMessage};
use crate::server::AppState;

use super::registration::CallRegistration;
use super::{close, text, UNPAIRED};

/// How long a call waits for the client's hello before it goes on without one.
pub(super) const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Try again later: the room has no seat for this call.
const ROOM_FULL: u16 = 1013;

/// What a call holds once the room seated it.
pub(super) struct Admitted {
    pub(super) session: String,
    /// The language the call's interface and the room's messages to it are in.
    pub(super) language: String,
    /// What the hello got wrong, as error events sent right after the session.
    pub(super) problems: Vec<Value>,
}

/// Waits for the client's first text frame, answering pings meanwhile. No hello in time is a call with no
/// conversation, in the machine's language.
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

/// Seats the call in the room, or refuses it on the socket.
pub(super) async fn admit(
    state: &AppState,
    socket: &mut WebSocket,
    device: String,
    hello: &Value,
    events: mpsc::Sender<Value>,
) -> Option<Admitted> {
    let (language, problem) =
        hello_language(hello.get("data").and_then(|data| data.get("ui_language")));
    let Ok(session) = state.room.join(device, language.clone(), events) else {
        let admission = state.room.admission(&language);
        refuse_full(socket, &admission).await;
        return None;
    };
    let problems = problem
        .map(|key| {
            let message = render(&LocalizedMessage::new(key), &language);
            json!({"type":"error","data":{"key":key,"message":message}})
        })
        .into_iter()
        .collect();
    Some(Admitted {
        session,
        language,
        problems,
    })
}

/// The interface language a hello asks for: `en` or `es`. Without one, the machine's; one the core does not
/// offer is refused with its key, and the call goes on in the machine's.
pub(super) fn hello_language(asked: Option<&Value>) -> (String, Option<&'static str>) {
    let fallback = ui_locale(&crate::runtime::system_language()).to_owned();
    match asked {
        None | Some(Value::Null) => (fallback, None),
        Some(Value::String(language)) if matches!(language.as_str(), "en" | "es") => {
            (language.clone(), None)
        }
        Some(_) => (fallback, Some("settings.ui_language_invalid")),
    }
}

/// Refuses a client the room has no seat for: the reason as a frame, then 1013 (try again later).
pub(super) async fn refuse_full(socket: &mut WebSocket, admission: &Value) {
    let refusal = json!({"type":"error","data":{"message":admission["message"],"reason":admission["reason"]}});
    let _ = socket.send(text(&refusal)).await;
    let _ = socket.send(close(ROOM_FULL, "")).await;
}
