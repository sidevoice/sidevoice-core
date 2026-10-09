//! The device's JSON control frames for its own session.

use serde_json::{json, Value};

use crate::control::room::RoomError;
use crate::messages::{render, LocalizedMessage};
use crate::server::call::admission::hello_language;
use crate::server::call::client_msg_id;

use super::Call;

impl Call {
    /// Applies a frame addressed to this session; anything else is ignored.
    pub(super) async fn on_client_text(&mut self, raw: &str) {
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return;
        };
        let data = value
            .get("data")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_default();
        if data.get("session_id").and_then(Value::as_str) != Some(self.session.as_str()) {
            return;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("voice-client-error") => {
                self.state.room.report_client_error(&data);
            }
            Some("voice-turn-trace") => trace_turn(&self.session, &data),
            Some("voice-user-turn") => self.once(&data, Self::user_turn).await,
            Some("voice-playback") => self.once(&data, Self::playback).await,
            Some("voice-settings") => self.update_settings(&data).await,
            _ => {}
        }
    }

    /// Takes a client message once: a repeat sent after a drop is acknowledged again and not applied.
    /// One without a `client_msg_id` is refused.
    async fn once(&mut self, data: &Value, apply: fn(&mut Self, &Value) -> Vec<Value>) {
        let Some(id) = client_msg_id(data).map(str::to_owned) else {
            let refusal = self.refusal("room.request_invalid", None);
            self.send(refusal).await;
            return;
        };
        // Claimed before it is applied: another call of this device sending it at the same time only acknowledges it.
        if self.state.seen.claim(&self.device, &id) {
            let events = apply(self, data);
            self.ack(&id).await;
            for event in events {
                self.send(event).await;
            }
        } else {
            self.ack(&id).await;
        }
    }

    /// The person's turn, as the call's voice module saw it. A turn starts here, which gives it its revision; it
    /// ends with the words it became, or with none (cancelled, or nothing said). A turn spoken while the call had
    /// no room becomes its own row when it arrives (`offline`).
    fn user_turn(&mut self, data: &Value) -> Vec<Value> {
        let room = self.state.room.clone();
        let id = client_msg_id(data);
        let result = match data["phase"].as_str() {
            Some("started") => room.begin_turn(&self.session).map(|turn| {
                json!({"type":"voice-user-turn","data":{"session_id":self.session,"phase":"started",
                    "revision":turn.revision,"thread_id":turn.thread_id}})
            }),
            Some("finished") if data["offline"].as_bool() == Some(true) => room
                .offline_input(
                    &self.session,
                    id.unwrap_or(""),
                    data["text"].as_str().unwrap_or(""),
                    data["started_at"].as_u64(),
                )
                .map(|_| Value::Null),
            Some(phase @ ("finished" | "cancelled")) => {
                let revision = data["revision"].as_u64().unwrap_or(0);
                let text = (phase == "finished")
                    .then(|| data["text"].as_str())
                    .flatten();
                room.finish_turn(&self.session, revision, text, &data["timings_ms"])
                    .map(|_| Value::Null)
            }
            _ => Err(RoomError::new(400, "room.request_invalid")),
        };
        match result {
            Ok(Value::Null) => Vec::new(),
            Ok(event) => vec![event],
            Err(error) => vec![self.refusal(error.key, id)],
        }
    }

    /// What became of a reply on this call: the room keeps how far each one was heard.
    fn playback(&mut self, data: &Value) -> Vec<Value> {
        // How far it was heard is optional; one that is given must be a count of characters.
        let heard_chars = match data.get("heard_chars") {
            None | Some(Value::Null) => Ok(None),
            Some(heard) => heard
                .as_u64()
                .map(Some)
                .ok_or_else(|| RoomError::new(400, "room.receipt_invalid")),
        };
        let result = heard_chars.and_then(|heard_chars| {
            self.state.room.playback(
                &self.session,
                data["utterance_id"].as_str().unwrap_or(""),
                data["status"].as_str().unwrap_or(""),
                data["reason"].as_str(),
                heard_chars,
                &data["timings_ms"],
            )
        });
        match result {
            Ok(()) => Vec::new(),
            Err(error) => vec![self.refusal(error.key, client_msg_id(data))],
        }
    }

    /// The call's interface language changed.
    async fn update_settings(&mut self, data: &Value) {
        match hello_language(data.get("ui_language")) {
            (language, None) if data.get("ui_language").is_some() => {
                self.state.room.set_language(&self.session, &language);
                self.language = language;
            }
            (_, Some(key)) => {
                let refusal = self.refusal(key, None);
                self.send(refusal).await;
            }
            _ => {}
        }
    }

    /// An error event in the call's language, naming the client message it answers when there is one.
    fn refusal(&self, key: &str, client_msg_id: Option<&str>) -> Value {
        let message = render(&LocalizedMessage::new(key), &self.language);
        let mut data = json!({"key":key,"message":message});
        if let Some(id) = client_msg_id {
            data["client_msg_id"] = json!(id);
        }
        json!({"type":"error","data":data})
    }
}

/// The browser opened the root span of a turn the room announced, and names it.
fn trace_turn(session: &str, data: &Value) {
    if let Some(telemetry) = crate::control::telemetry::shared() {
        telemetry.turn_context(
            session,
            data["thread_id"].as_str().unwrap_or(""),
            data["revision"].as_u64().unwrap_or(0),
            data["traceparent"].as_str(),
        );
    }
}
