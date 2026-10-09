//! The device's JSON control frames for its own session.

use serde_json::{json, Value};

use crate::server::call::admission::{runtime_refusal, unavailable_refusal};
use crate::server::call::client_msg_id;
use crate::server::media::Source;

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
            Some("voice-audio-health") => audio_health(&self.session, &data),
            Some("voice-media") => self.select_media(&data).await,
            Some("voice-transcript") => self.transcript(&data, false).await,
            Some("voice-transcript-error") => self.transcript(&data, true).await,
            Some("voice-catchup") => self.catchup(&data).await,
            Some("voice-settings") => self.update_settings(&data).await,
            Some("voice-stt-ready") => self.runtime_ready(&data).await,
            _ => {}
        }
    }

    /// A transcript the page may send again after a drop: taken once, acknowledged every time. One
    /// that no recognition of this call is waiting for (asked by a session that is gone, or answered
    /// after its wait ran out) is still the person's words, and reaches the conversation as text.
    async fn transcript(&mut self, data: &Value, error: bool) {
        let Some(id) = client_msg_id(data).map(str::to_owned) else {
            self.media.transcript(data, error, &self.session);
            return;
        };
        if self.state.seen.answer(&self.device, &id).is_none() {
            if !self.media.transcript(data, error, &self.session) && !error {
                self.turns.loose_transcript(data).await;
            }
            self.state.seen.remember(&self.device, &id, Value::Null);
        }
        self.ack(&id).await;
    }

    /// A catch-up slice; a whole catch-up already taken is acknowledged again, not recognised again.
    async fn catchup(&mut self, data: &Value) {
        let id = client_msg_id(data).map(str::to_owned);
        if let Some(id) = &id {
            if self.state.seen.answer(&self.device, id).is_some() {
                if data["final"].as_bool() == Some(true) {
                    self.ack(id).await;
                }
                return;
            }
        }
        if self.turns.catchup_slice(data).await {
            if let Some(id) = id {
                self.state.seen.remember(&self.device, &id, Value::Null);
                self.ack(&id).await;
            }
        }
    }

    // `&mut` keeps the future `Send`: the call socket is not `Sync`.
    async fn select_media(&mut self, data: &Value) {
        match data.get("path").and_then(Value::as_str) {
            Some("socket") => {
                self.media.select(Source::Socket);
                self.media.close_rtc().await;
            }
            Some("webrtc") => self.media.select(Source::WebRtc),
            _ => {}
        }
    }

    /// Takes the voice, language and timing parts of new settings; detection and
    /// transcription stay as the call started.
    async fn update_settings(&mut self, data: &Value) {
        let loaded = crate::models::settings_from(data.get("settings"), &self.defaults);
        if let Some(issue) = loaded.issue {
            let refusal = json!({"type":"error","data":{"message":issue}});
            self.send(refusal).await;
            return;
        }
        if let Some(refusal) = unavailable_refusal(&self.state, &loaded.settings) {
            self.send(refusal).await;
            return;
        }
        let settings = loaded.settings;
        self.state
            .room
            .set_language(&self.session, &settings.ui_language);
        self.settings.tts = settings.tts;
        self.settings.ui_language = settings.ui_language;
        self.settings.audio_grace_seconds = settings.audio_grace_seconds;
        self.state
            .room
            .set_audio_grace(&self.session, settings.audio_grace_seconds);
        self.state
            .call_settings
            .lock()
            .expect("call settings lock")
            .insert(self.session.clone(), self.settings.clone());
    }

    /// Records the transcription runtime the browser loaded, for the room's stats only.
    async fn runtime_ready(&mut self, data: &Value) {
        match crate::models::browser_runtime(Some(data)) {
            Ok(Some(runtime)) => {
                if let Some(view) = self.transcription.as_object_mut() {
                    view.extend(runtime);
                }
                self.state
                    .room
                    .set_transcription(&self.session, self.transcription.clone());
            }
            Ok(None) => {}
            Err(problem) => {
                let refusal = json!({"type":"error","data":runtime_refusal(&problem, &self.settings.ui_language)});
                self.send(refusal).await;
            }
        }
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

/// What the browser's audio output did, as an event on the call's span.
fn audio_health(session: &str, data: &Value) {
    if let Some(telemetry) = crate::control::telemetry::shared() {
        let health = &data["health"];
        telemetry.audio_event(
            session,
            data["reason"].as_str().unwrap_or(""),
            &json!({"sidevoice.audio_output": health["output"],
                "sidevoice.audio_context": health["context"], "sidevoice.stalls": health["stalls"]}),
        );
    }
}
