//! The device's JSON control frames for its own session.

use serde_json::{json, Value};

use crate::server::call::admission::unavailable_refusal;
use crate::server::call::text;
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
            Some("voice-media") => self.select_media(&data).await,
            Some("voice-transcript") => self.media.transcript(&data, false, &self.session),
            Some("voice-transcript-error") => self.media.transcript(&data, true, &self.session),
            Some("voice-catchup") => self.turns.catchup_slice(&data).await,
            Some("voice-settings") => self.update_settings(&data).await,
            _ => {}
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
            let _ = self.socket.send(text(&refusal)).await;
            return;
        }
        if let Some(refusal) = unavailable_refusal(&self.state, &loaded.settings) {
            let _ = self.socket.send(text(&refusal)).await;
            return;
        }
        let settings = loaded.settings;
        self.state
            .room
            .set_language(&self.session, &settings.ui_language);
        self.settings.tts = settings.tts;
        self.settings.ui_language = settings.ui_language;
        self.settings.audio_grace_seconds = settings.audio_grace_seconds;
        self.settings.replay_on_return_seconds = settings.replay_on_return_seconds;
        self.state
            .call_settings
            .lock()
            .expect("call settings lock")
            .insert(self.session.clone(), self.settings.clone());
    }
}
