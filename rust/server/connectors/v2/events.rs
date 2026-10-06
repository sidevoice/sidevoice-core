//! The connector's Socket.IO events, each forwarded to the room while this
//! connection is still the connector's current peer.

use std::sync::Arc;

use serde_json::{json, Value};
use socketioxide::extract::{AckSender, SocketRef, State, TryData};

use crate::control::room::Room;
use crate::messages::{render, LocalizedMessage};
use crate::server::AppState;

use crate::server::connectors::{echo_ids, register_binding, Link, Notification};

const DISCONNECTED: &str = "room.connector_disconnected";

/// The connector's words for a v2 answer: v2 connectors log and show `error` as it comes, so a room key becomes its
/// English sentence (what reaches the agent is English). v3 keeps the stable key.
pub(super) fn readable(mut answer: Value) -> Value {
    if let Some(key) = answer.get("error").and_then(Value::as_str) {
        answer["error"] = json!(render(&LocalizedMessage::new(key), "en"));
    }
    answer
}

pub(super) fn register(socket: &SocketRef, room: &Arc<Room>, link: &Link) {
    on_register(socket, room.clone(), link.clone());
    for notification in Notification::ALL {
        on_notification(socket, room.clone(), link.clone(), notification);
    }
    on_speech(socket, room.clone(), link.clone());
    on_pairing_code(socket, link.clone());
}

fn on_register(socket: &SocketRef, room: Arc<Room>, link: Link) {
    socket.on(
        "binding.register",
        move |TryData(data): TryData<Value>, ack: AckSender| {
            let room = room.clone();
            let link = link.clone();
            async move {
                let answer = if link.current(&room) {
                    register_binding(&room, &link.cid, &data.unwrap_or(json!({})))
                } else {
                    json!({ "error": DISCONNECTED })
                };
                let _ = ack.send(&readable(answer));
            }
        },
    );
}

fn on_notification(socket: &SocketRef, room: Arc<Room>, link: Link, notification: Notification) {
    socket.on(
        notification.method(),
        move |TryData(data): TryData<Value>| {
            let room = room.clone();
            let link = link.clone();
            async move {
                if link.current(&room) {
                    notification.apply(&room, &link.cid, &data.unwrap_or(json!({})));
                }
            }
        },
    );
}

fn on_speech(socket: &SocketRef, room: Arc<Room>, link: Link) {
    socket.on(
        "speech.publish",
        move |TryData(data): TryData<Value>, ack: AckSender| {
            let room = room.clone();
            let link = link.clone();
            async move {
                let data = data.unwrap_or(json!({}));
                let answer = if link.current(&room) {
                    room.connector_speech(&link.cid, &data, false)
                } else {
                    json!({"status":"rejected","error":DISCONNECTED})
                };
                let _ = ack.send(&echo_ids(readable(answer), &data, &["event_id"]));
            }
        },
    );
}

fn on_pairing_code(socket: &SocketRef, link: Link) {
    socket.on(
        "device.pairing_code",
        move |ack: AckSender, State(state): State<Arc<AppState>>| {
            let link = link.clone();
            async move {
                if link.current(&state.room) {
                    let _ = ack.send(&state.issue_code());
                }
            }
        },
    );
}
