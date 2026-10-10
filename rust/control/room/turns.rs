//! Voice turns: the person's spoken turn in a call, as the call's voice module reports it. The module names each turn
//! (`turn_id`); the room gives it a revision when it starts, the turn's boundary, and the focus its words will go to.
//! A turn ends with the words it became, or with none. Several may wait for their words at once: the person may start
//! the next turn, or move to another conversation, while the last one is still transcribed.
use serde_json::{json, Value};

use super::browsers::MAX_OPEN_TURNS;
use super::error::RoomError;
use super::input::MAX_INPUT_BYTES;
use super::latency::{latency_now_micros, LatencyEvent};
use super::Room;

/// The longest turn id the room takes, in bytes.
const MAX_TURN_ID: usize = 64;

/// A turn as the room took it when it started: the client's id for it, and the revision and focus captured then.
#[derive(Clone, Debug)]
pub struct VoiceTurn {
    pub session_id: String,
    /// The client's id for the turn, the same in every phase.
    pub turn_id: String,
    /// The room's revision for the turn: a reply written before it answers an older turn.
    pub revision: u64,
    pub thread_id: Option<String>,
    pub binding_id: Option<String>,
    pub title: Option<String>,
    pub(super) language: String,
    /// The person cancelled it: its words, when they come, are dropped.
    pub(super) cancelled: bool,
}

/// Whether `turn_id` can name a turn: 1 to [`MAX_TURN_ID`] bytes.
pub(super) fn valid_turn_id(turn_id: &str) -> bool {
    !turn_id.is_empty() && turn_id.len() <= MAX_TURN_ID
}

impl Room {
    /// The person started speaking turn `turn_id` in call `sid`: a new revision, and the focus the turn's words will
    /// go to. A turn the call already holds is not started again. The replies the call was sent before it and has not
    /// started playing answer an older turn: they are withdrawn (`newer_turn`). One playing is the call's to cut, and
    /// a replay the person asked for is never stale.
    pub fn begin_turn(&self, sid: &str, turn_id: &str) -> Result<VoiceTurn, RoomError> {
        if !valid_turn_id(turn_id) {
            return Err(RoomError::new(400, "room.request_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let Some(c) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if c.turns.iter().any(|turn| turn.turn_id == turn_id) {
            return Err(RoomError::new(409, "room.request_invalid"));
        }
        // The turns already open keep their words' destination: a new one waits until one of them ends.
        if c.turns.len() >= MAX_OPEN_TURNS {
            return Err(RoomError::new(429, "room.turns_full"));
        }
        let revision = c.next_revision();
        c.turn_revision = revision;
        c.speaking = true;
        c.open_turn = Some(revision);
        let bound = c.bound_target();
        let turn = VoiceTurn {
            session_id: sid.into(),
            turn_id: turn_id.into(),
            revision,
            thread_id: bound.map(|t| t.thread.clone()),
            binding_id: bound.map(|t| t.binding_id.clone()),
            title: c.target.as_ref().and_then(|t| t.title.clone()),
            language: c.language.clone(),
            cancelled: false,
        };
        c.turns.push_back(turn.clone());
        inner.withdraw(sid, "newer_turn", |record, (sent_at, status)| {
            status == "queued" && *sent_at < revision && !record.requested
        });
        Ok(turn)
    }

    /// Turn `turn_id` of call `sid` ended. With `text`, its words go to the conversation it was spoken to, as a
    /// message; without (cancelled, or nothing said), it just ends. `timings` are the call's own measures of it.
    /// A refusal the call may retry (`room.text_too_long`, `room.input_backlog_full`) leaves the turn open, with the
    /// revision and focus it started with; the turn ends only with its words taken, or with none.
    pub fn finish_turn(
        &self,
        sid: &str,
        turn_id: &str,
        text: Option<&str>,
        timings: &Value,
    ) -> Result<Value, RoomError> {
        if text.is_some_and(|text| text.len() > MAX_INPUT_BYTES) {
            return Err(RoomError::new(413, "room.text_too_long"));
        }
        let text = text.filter(|text| !text.trim().is_empty());
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let Some(index) = browser
            .turns
            .iter()
            .position(|turn| turn.turn_id == turn_id)
        else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        let open = &browser.turns[index];
        if let Some(text) = text.filter(|_| !open.cancelled && open.thread_id.is_some()) {
            inner.room_for(text)?;
        }
        let browser = inner
            .browsers
            .get_mut(sid)
            .expect("the call, under the same lock");
        let turn = browser.turns.remove(index).expect("the turn found");
        if browser.open_turn == Some(turn.revision) {
            browser.speaking = false;
            browser.open_turn = None;
        }
        let revision = turn.revision;
        let Some(text) = text else {
            return Ok(json!({"accepted":false,"revision":revision}));
        };
        if let Some(thread) = turn.thread_id.as_deref() {
            inner.mark_latency(
                sid,
                thread,
                revision,
                None,
                LatencyEvent::Transcript,
                latency_now_micros(),
            );
            if let Some(timings) = timings.as_object() {
                for (name, milliseconds) in timings {
                    if let Some(milliseconds) = milliseconds.as_f64() {
                        inner.record_latency_duration(
                            sid,
                            thread,
                            revision,
                            None,
                            name,
                            milliseconds,
                        );
                    }
                }
            }
        }
        inner.queue_turn(&turn, text)
    }

    /// The person cancelled turn `turn_id`, from the page, while speaking it or before its words came: its words,
    /// when they come, are dropped.
    pub fn cancel_input(&self, sid: &str, turn_id: &str) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        let Some(turn) = browser
            .turns
            .iter_mut()
            .find(|turn| turn.turn_id == turn_id && !turn.cancelled)
        else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        turn.cancelled = true;
        let thread = turn.thread_id.clone();
        browser.notify(json!({"type":"voice-user-turn","data":{"session_id":sid,
            "phase":"cancelled","turn_id":turn_id,"thread_id":thread}}));
        Ok(json!({"status":"cancelled"}))
    }
}
