//! User input entering the journal: typed text, completed voice turns and offline audio.
use serde_json::{json, Value};

use super::error::RoomError;
use super::journal::{InputRef, Row};
use super::latency::{latency_now_micros, LatencyEvent};
use super::turns::VoiceTurn;
use super::util::{id, millis, seconds};
use super::{Inner, Room};

struct InputDraft<'a> {
    row_id: String,
    text: &'a str,
    session_id: &'a str,
    revision: u64,
    thread_id: &'a str,
    binding_id: &'a str,
    title: Option<String>,
    language: &'a str,
    message_id: &'a str,
    offline: Option<&'a str>,
    time: Option<u64>,
}

impl Room {
    pub fn send_text(
        &self,
        text: &str,
        sid: &str,
        thread: &str,
        bid: &str,
        message_id: &str,
    ) -> Result<Value, RoomError> {
        let row_id = format!("{sid}:user-text:{message_id}");
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(row) = inner.journal.find(&row_id) {
            return repeated(row, text, row.thread == thread);
        }
        let Some(c) = inner.browsers.get(sid) else {
            return Err(RoomError::new(409, "room.focus_changed"));
        };
        if c.target
            .as_ref()
            .is_none_or(|t| t.thread != thread || t.binding_id != bid)
        {
            return Err(RoomError::new(409, "room.focus_changed"));
        }
        if text.trim().is_empty() {
            return Err(RoomError::new(422, "room.text_empty"));
        }
        let revision = c.revision;
        let language = c.language.clone();
        let title = c.target.as_ref().and_then(|t| t.title.clone());
        Ok(inner.queue_input(InputDraft {
            row_id,
            text,
            session_id: sid,
            revision,
            thread_id: thread,
            binding_id: bid,
            title,
            language: &language,
            message_id,
            offline: None,
            time: None,
        }))
    }
    /// Commit a completed transcript to the same memory journal/outbox as typed input.
    /// The captured focus may differ from today's selection after a mid-turn switch.
    pub fn queue_voice_input(&self, turn: &VoiceTurn, text: &str) -> Result<Value, RoomError> {
        if text.trim().is_empty() {
            return Err(RoomError::new(422, "room.text_empty"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        if inner
            .browsers
            .get(&turn.session_id)
            .is_some_and(|browser| browser.cancelled_turn == Some(turn.revision))
        {
            return Err(RoomError::new(409, "room.input_ended"));
        }
        if !inner.browsers.is_recent(&turn.session_id) {
            return Err(RoomError::new(409, "room.focus_changed"));
        }
        let row_id = format!("{}:user-turn:{}", turn.session_id, turn.revision);
        if let Some(row) = inner.journal.find(&row_id) {
            return repeated(
                row,
                text,
                turn.thread_id.as_deref() == Some(row.thread.as_str()),
            );
        }
        let (Some(thread), Some(bid)) = (turn.thread_id.as_deref(), turn.binding_id.as_deref())
        else {
            return Ok(inner.not_sent(&turn.session_id, row_id, turn.revision));
        };
        let message_id = id();
        Ok(inner.queue_input(InputDraft {
            row_id,
            text,
            session_id: &turn.session_id,
            revision: turn.revision,
            thread_id: thread,
            binding_id: bid,
            title: turn.title.clone(),
            language: &turn.language,
            message_id: &message_id,
            offline: None,
            time: None,
        }))
    }
    /// Offline audio is a separate input row, never a live voice revision.
    pub fn offline_target(&self, sid: &str) -> Option<VoiceTurn> {
        let inner = self.inner.lock().expect("room lock");
        let browser = inner.browsers.get(sid)?;
        let target = browser.bound_target();
        Some(VoiceTurn {
            session_id: sid.into(),
            revision: 0,
            thread_id: target.map(|target| target.thread.clone()),
            binding_id: target.map(|target| target.binding_id.clone()),
            title: target.and_then(|target| target.title.clone()),
            language: browser.language.clone(),
        })
    }
    pub fn queue_offline_input(
        &self,
        target: &VoiceTurn,
        row_id: &str,
        text: &str,
        offline: &str,
        time: Option<u64>,
    ) -> Result<Value, RoomError> {
        if text.trim().is_empty() {
            return Err(RoomError::new(422, "room.text_empty"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let sid = target.session_id.as_str();
        if !inner.browsers.contains(sid) {
            return Err(RoomError::new(409, "room.browser_absent"));
        }
        let (Some(thread), Some(binding)) =
            (target.thread_id.as_deref(), target.binding_id.as_deref())
        else {
            return Ok(inner.not_sent(sid, row_id.into(), 0));
        };
        let message_id = id();
        Ok(inner.queue_input(InputDraft {
            row_id: row_id.into(),
            text,
            session_id: sid,
            revision: 0,
            thread_id: thread,
            binding_id: binding,
            title: target.title.clone(),
            language: &target.language,
            message_id: &message_id,
            offline: Some(offline),
            time,
        }))
    }
}

/// The answer to input whose row already exists: accepted again if it is the same words for
/// the same conversation, a conflict otherwise.
fn repeated(row: &Row, text: &str, same_thread: bool) -> Result<Value, RoomError> {
    if row.text == text && same_thread {
        Ok(json!({"accepted":true,"id":row.id,"revision":row.revision}))
    } else {
        Err(RoomError::new(409, "room.message_conflict"))
    }
}

impl Inner {
    fn queue_input(&mut self, draft: InputDraft<'_>) -> Value {
        let InputDraft {
            row_id,
            text,
            session_id,
            revision,
            thread_id,
            binding_id,
            title,
            language,
            message_id,
            offline,
            time,
        } = draft;
        let payload = json!({"thread_id":thread_id,"text":text,"message_id":message_id,"session_id":session_id,
            "history_id":row_id,"revision":revision,"binding_id":binding_id,"title":title});
        let row = Row {
            id: row_id,
            thread: thread_id.into(),
            role: "user",
            text: text.into(),
            name: Some(crate::messages::render(
                &crate::messages::LocalizedMessage::new("room.you"),
                language,
            )),
            session: session_id.into(),
            revision,
            time: time.unwrap_or_else(millis),
            status: "pending".into(),
            offline: offline.map(|value| json!(value)),
            payload: Some(payload),
            queued_at: seconds(),
            ..Row::default()
        };
        let input = row.input_ref();
        self.append_row(row);
        self.mark_latency(
            session_id,
            thread_id,
            revision,
            None,
            LatencyEvent::Queued,
            latency_now_micros(),
        );
        self.browsers.input_receipt(&input, "pending");
        json!({"accepted":true,"id":input.id,"revision":revision})
    }
    /// Input captured without a conversation to send it to: it is never queued.
    fn not_sent(&self, sid: &str, row_id: String, revision: u64) -> Value {
        let input = InputRef {
            session: sid.into(),
            id: row_id,
            thread: None,
            revision,
        };
        self.browsers.input_receipt(&input, "not_sent");
        json!({"accepted":false,"id":input.id,"revision":revision,"status":"not_sent"})
    }
}
