//! What the turn owner tells the browser: user-turn phases and localized errors.

use serde_json::{json, Value};

use super::super::recognition::SttFailure;
use super::TurnOwner;
use crate::{
    control::room::VoiceTurn,
    messages::{render, LocalizedMessage},
};

pub(super) fn started(turn: &VoiceTurn) -> Value {
    user_turn("started", turn)
}

pub(super) fn cancelled(turn: &VoiceTurn, text: &str) -> Value {
    let mut data = user_turn("cancelled", turn);
    data["text"] = json!(text);
    data
}

/// A transcript held back to be joined with the next one on the same focus.
pub(super) fn merged(turn: &VoiceTurn, text: &str) -> Value {
    let mut data = cancelled(turn, text);
    data["merged"] = json!(true);
    data
}

pub(super) fn finished(turn: &VoiceTurn, text: &str) -> Value {
    let mut data = user_turn("finished", turn);
    data["text"] = json!(text);
    data
}

fn user_turn(phase: &str, turn: &VoiceTurn) -> Value {
    json!({"phase":phase,"revision":turn.revision,"thread_id":turn.thread_id})
}

pub(super) fn failure_key(error: SttFailure, offline: bool) -> &'static str {
    match (offline, error) {
        (true, _) => "voice.catchup_transcription_failed",
        (false, SttFailure::Timeout) => "voice.transcription_timeout",
        (false, _) => "voice.transcription_failed",
    }
}

impl TurnOwner {
    pub(super) async fn announce(&self, data: Value) {
        let _ = self
            .events
            .send(json!({"type":"voice-user-turn","data":data}))
            .await;
    }

    /// Ends a turn that produced no transcript.
    pub(super) async fn abandon(&self, turn: &VoiceTurn) {
        self.announce(cancelled(turn, "")).await;
        self.room.finish_turn(&self.session, turn.revision);
    }

    pub(super) async fn report_failure(&self, error: SttFailure, offline: bool) {
        self.report(failure_key(error, offline)).await;
    }

    pub(super) async fn report(&self, key: &str) {
        let message = render(&LocalizedMessage::new(key), &self.settings.ui_language);
        let _ = self
            .events
            .send(json!({"type":"error","data":{"message":message}}))
            .await;
    }
}
