//! The device's JSON control frames for its own session.

use serde_json::{json, Value};

use crate::control::room::{Continuation, RoomError};
use crate::messages::{render, LocalizedMessage};
use crate::server::call::admission::offered_language;
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
            Some("reply-hearing") => self.once(&data, Self::reply_hearing).await,
            Some("voice-speech") => self.once(&data, Self::speech).await,
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

    /// The person's turn, as the call's voice module saw it, named by it (`turn_id`) in every phase; the call sends
    /// no revision. A turn starts here, which gives it its revision, the turn's boundary; it
    /// ends with the words it became, or with none (cancelled, or nothing said). A turn spoken while the call had
    /// no room becomes its own row when it arrives (`offline`).
    fn user_turn(&mut self, data: &Value) -> Vec<Value> {
        let room = self.state.room.clone();
        let id = client_msg_id(data);
        let turn_id = data["turn_id"].as_str().unwrap_or("");
        // The turn's boundary, as the call learns it: the revision the room gave the turn, and where its words go.
        let started = |revision: &Value, epoch: &Value, thread: &Value| {
            json!({"type":"voice-user-turn","data":{"session_id":self.session,"phase":"started",
                "turn_id":turn_id,"revision":revision,"epoch":epoch,"thread_id":thread}})
        };
        let result = match data["phase"].as_str() {
            Some("started") => data["thread_id"]
                .as_str()
                .zip(data["binding_id"].as_str())
                .ok_or_else(|| RoomError::new(400, "room.request_invalid"))
                .and_then(|(thread, binding)| {
                    room.begin_turn_with_snapshot(
                        &self.session,
                        turn_id,
                        thread,
                        binding,
                        &data["playback_at_turn_start"],
                    )
                })
                .map(|turn| {
                    started(
                        &json!(turn.revision),
                        &json!(turn.epoch),
                        &json!(turn.thread_id),
                    )
                }),
            // A turn spoken offline arrives finished: once taken, its boundary is said as a started turn's is.
            Some("finished") if data["offline"].as_bool() == Some(true) => room
                .offline_input_captured_with_evidence(
                    &self.session,
                    turn_id,
                    data["text"].as_str().unwrap_or(""),
                    data["started_at"].as_u64(),
                    data["thread_id"].as_str().unwrap_or(""),
                    data["binding_id"].as_str().unwrap_or(""),
                    &data["playback_at_turn_start"],
                    &data["not_played_at_turn_start"],
                    data["not_played_at_turn_start_truncated"]
                        .as_bool()
                        .unwrap_or(false),
                )
                .map(|taken| {
                    if taken["accepted"] == true {
                        started(&taken["revision"], &taken["epoch"], &taken["thread_id"])
                    } else {
                        Value::Null
                    }
                }),
            Some(phase @ ("finished" | "cancelled")) => {
                let text = (phase == "finished")
                    .then(|| data["text"].as_str())
                    .flatten();
                room.finish_turn_with_evidence(
                    &self.session,
                    turn_id,
                    text,
                    &data["timings_ms"],
                    if data["merged"].as_bool() == Some(true) {
                        Continuation::Merged
                    } else {
                        Continuation::Standalone
                    },
                    &data["playback_at_turn_start"],
                    &data["not_played_at_turn_start"],
                    data["not_played_at_turn_start_truncated"]
                        .as_bool()
                        .unwrap_or(false),
                )
                .map(|_| Value::Null)
            }
            // A phase the room does not know is taken and changes nothing; a turn report without one is refused.
            Some(phase) if !phase.is_empty() => Ok(Value::Null),
            _ => Err(RoomError::new(400, "room.request_invalid")),
        };
        match result {
            Ok(Value::Null) => Vec::new(),
            Ok(event) => vec![event],
            Err(error) => vec![self.refusal(error.key, id)],
        }
    }

    /// Physical playback state and diagnostic extent; semantic hearing is reported separately.
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
            if let Some(attempt_id) = data["attempt_id"].as_str() {
                return self.state.room.contract_playback(
                    &self.session,
                    data["utterance_id"].as_str().unwrap_or(""),
                    attempt_id,
                    data["status"].as_str().unwrap_or(""),
                    data["hearing"].as_str(),
                    heard_chars,
                    data["reason"].as_str(),
                );
            }
            self.state.room.playback_transport(
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

    fn speech(&mut self, data: &Value) -> Vec<Value> {
        match self.state.room.set_speech(
            &self.session,
            data["on"].as_bool().unwrap_or(false),
            data["reason"].as_str(),
        ) {
            Ok(_) => Vec::new(),
            Err(error) => vec![self.refusal(error.key, client_msg_id(data))],
        }
    }

    fn reply_hearing(&mut self, data: &Value) -> Vec<Value> {
        let heard_chars = match data.get("heard_chars") {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .map(Some)
                .ok_or_else(|| RoomError::new(400, "room.receipt_invalid")),
        };
        let result = heard_chars.and_then(|heard_chars| {
            self.state.room.reply_hearing(
                &self.session,
                data["history_id"].as_str().unwrap_or(""),
                data["utterance_id"].as_str().unwrap_or(""),
                data["outcome"].as_str().unwrap_or(""),
                heard_chars,
            )
        });
        match result {
            Ok(()) => Vec::new(),
            Err(error) => vec![self.refusal(error.key, client_msg_id(data))],
        }
    }

    /// The call's interface language changed. One the core does not offer keeps the current language.
    async fn update_settings(&mut self, data: &Value) {
        match offered_language(data.get("ui_language")) {
            Ok(Some(language)) => {
                self.state.room.set_language(&self.session, &language);
                self.language = language;
            }
            Ok(None) => {}
            Err(key) => {
                let refusal = self.refusal(key, None);
                self.send(refusal).await;
            }
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
