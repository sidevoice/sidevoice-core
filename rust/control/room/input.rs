//! User input entering the journal: typed text, completed voice turns, and turns spoken offline.
use serde_json::{json, Value};

use super::error::RoomError;
use super::journal::{InputRef, Row};
use super::latency::{latency_now_micros, LatencyEvent};
use super::turns::{valid_turn_id, VoiceTurn};
use super::util::{id, millis, seconds};
use super::{Inner, Room};

/// The longest message the room takes, in bytes of its words: typed, spoken, or spoken offline.
pub(super) const MAX_INPUT_BYTES: usize = 12_000;
/// How many messages may wait at once to reach their agents, and how many bytes of words between them.
const MAX_WAITING_INPUT: usize = 256;
const MAX_WAITING_BYTES: usize = 1024 * 1024;

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
    /// The client's id for the turn the words were spoken in; none for typed text.
    turn_id: Option<&'a str>,
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
        words(text)?;
        let revision = c.revision;
        let language = c.language.clone();
        let title = c.target.as_ref().and_then(|t| t.title.clone());
        inner.queue_input(InputDraft {
            row_id,
            text,
            session_id: sid,
            revision,
            thread_id: thread,
            binding_id: bid,
            title,
            language: &language,
            message_id,
            turn_id: None,
            offline: None,
            time: None,
        })
    }
    /// Commit a completed transcript to the same memory journal/outbox as typed input.
    /// The captured focus may differ from today's selection after a mid-turn switch.
    pub fn queue_voice_input(&self, turn: &VoiceTurn, text: &str) -> Result<Value, RoomError> {
        self.inner.lock().expect("room lock").queue_turn(turn, text)
    }
    /// Turn `turn_id`, spoken and transcribed while the call had no room, is its own input row, sent to the
    /// conversation the call is on now. It takes the call's next revision, as a turn does when it starts: it is newer
    /// than every turn before it, so a reply to one of those is superseded by it. The same turn sent again is taken
    /// once.
    pub fn offline_input(
        &self,
        sid: &str,
        turn_id: &str,
        text: &str,
        time: Option<u64>,
    ) -> Result<Value, RoomError> {
        if !valid_turn_id(turn_id) {
            return Err(RoomError::new(400, "room.request_invalid"));
        }
        words(text)?;
        let row_id = turn_row(sid, turn_id);
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(row) = inner.journal.find(&row_id) {
            return repeated(row, text, true);
        }
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let language = browser.language.clone();
        let Some(target) = browser.bound_target().cloned() else {
            return Ok(inner.not_sent(sid, row_id, 0, turn_id));
        };
        inner.room_for(text)?;
        let browser = inner
            .browsers
            .get_mut(sid)
            .expect("the call, under the same lock");
        let revision = browser.next_revision();
        browser.turn_revision = revision;
        let (thread, binding) = (target.thread.as_str(), target.binding_id.as_str());
        let message_id = id();
        inner.queue_input(InputDraft {
            row_id,
            text,
            session_id: sid,
            revision,
            thread_id: thread,
            binding_id: binding,
            title: target.title.clone(),
            language: &language,
            message_id: &message_id,
            turn_id: Some(turn_id),
            offline: Some("offline"),
            time,
        })
    }
}

/// The journal row of call `sid`'s turn `turn_id`, live or offline.
fn turn_row(sid: &str, turn_id: &str) -> String {
    format!("{sid}:user-turn:{turn_id}")
}

/// Refuses words that cannot be a message: none, or more than [`MAX_INPUT_BYTES`].
fn words(text: &str) -> Result<(), RoomError> {
    if text.trim().is_empty() {
        return Err(RoomError::new(422, "room.text_empty"));
    }
    if text.len() > MAX_INPUT_BYTES {
        return Err(RoomError::new(413, "room.text_too_long"));
    }
    Ok(())
}

/// The answer to input whose row already exists: accepted again if it is the same words for
/// the same conversation, a conflict otherwise.
fn repeated(row: &Row, text: &str, same_thread: bool) -> Result<Value, RoomError> {
    if row.text == text && same_thread {
        Ok(json!({"accepted":true,"id":row.id,"revision":row.revision,"thread_id":row.thread}))
    } else {
        Err(RoomError::new(409, "room.message_conflict"))
    }
}

impl Inner {
    /// Commits turn `turn`'s words to the journal and its outbox; the turn must be one the call ended.
    pub(super) fn queue_turn(&mut self, turn: &VoiceTurn, text: &str) -> Result<Value, RoomError> {
        words(text)?;
        if turn.cancelled {
            return Err(RoomError::new(409, "room.input_ended"));
        }
        if !self.browsers.is_recent(&turn.session_id) {
            return Err(RoomError::new(409, "room.focus_changed"));
        }
        let row_id = turn_row(&turn.session_id, &turn.turn_id);
        if let Some(row) = self.journal.find(&row_id) {
            return repeated(
                row,
                text,
                turn.thread_id.as_deref() == Some(row.thread.as_str()),
            );
        }
        let (Some(thread), Some(bid)) = (turn.thread_id.as_deref(), turn.binding_id.as_deref())
        else {
            return Ok(self.not_sent(&turn.session_id, row_id, turn.revision, &turn.turn_id));
        };
        let message_id = id();
        self.queue_input(InputDraft {
            row_id,
            text,
            session_id: &turn.session_id,
            revision: turn.revision,
            thread_id: thread,
            binding_id: bid,
            title: turn.title.clone(),
            language: &turn.language,
            message_id: &message_id,
            turn_id: Some(&turn.turn_id),
            offline: None,
            time: None,
        })
    }
    fn queue_input(&mut self, draft: InputDraft<'_>) -> Result<Value, RoomError> {
        self.room_for(draft.text)?;
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
            turn_id,
            offline,
            time,
        } = draft;
        let mut payload = json!({"thread_id":thread_id,"text":text,"message_id":message_id,"session_id":session_id,
            "history_id":row_id,"revision":revision,"binding_id":binding_id,"title":title});
        // The message takes with it what the person did not hear, so the agent is told once.
        if let Some(unheard) = self.unheard.take(thread_id, &self.journal) {
            payload["unheard"] = unheard;
        }
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
            turn_id: turn_id.map(str::to_owned),
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
        Ok(json!({"accepted":true,"id":input.id,"revision":revision,"thread_id":thread_id}))
    }
    /// Refuses `text` when the messages already waiting to reach their agents are as many, or as long, as the room
    /// keeps: a message is only taken when it can wait for its agent.
    pub(super) fn room_for(&self, text: &str) -> Result<(), RoomError> {
        let (count, bytes) = self.journal.waiting_input();
        if count >= MAX_WAITING_INPUT || bytes + text.len() > MAX_WAITING_BYTES {
            return Err(RoomError::new(429, "room.input_backlog_full"));
        }
        Ok(())
    }
    /// Turn `turn_id`'s words, captured without a conversation to send them to: they are never queued.
    fn not_sent(&self, sid: &str, row_id: String, revision: u64, turn_id: &str) -> Value {
        let input = InputRef {
            session: sid.into(),
            id: row_id,
            thread: None,
            revision,
            turn_id: Some(turn_id.to_owned()),
        };
        self.browsers.input_receipt(&input, "not_sent");
        json!({"accepted":false,"id":input.id,"revision":revision,"status":"not_sent"})
    }
}
