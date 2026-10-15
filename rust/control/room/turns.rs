//! Voice turns: the person's spoken turn in a call, as the call's voice module reports it. The module names each turn
//! (`turn_id`); the room gives it a revision when it starts, the turn's boundary, and the focus its words will go to.
//! A turn ends with the words it became, or with none. Several may wait for their words at once: the person may start
//! the next turn, or move to another conversation, while the last one is still transcribed.
use serde_json::{json, Value};

use super::browsers::MAX_OPEN_TURNS;
use super::error::RoomError;
use super::input::MAX_INPUT_BYTES;
use super::latency::{latency_now_micros, LatencyEvent};
use super::{Inner, Room};

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
    /// Conversation-local speech boundary, independent of the call revision.
    pub epoch: u64,
    pub thread_id: Option<String>,
    pub binding_id: Option<String>,
    pub title: Option<String>,
    pub(super) language: String,
    /// The person cancelled it: its words, when they come, are dropped.
    pub(super) cancelled: bool,
}

/// Whether this terminal report continues into a merged turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Continuation {
    Standalone,
    Merged,
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
    pub fn begin_turn(
        &self,
        sid: &str,
        turn_id: &str,
        thread_id: &str,
        binding_id: &str,
    ) -> Result<VoiceTurn, RoomError> {
        if binding_id.is_empty() && !thread_id.is_empty() {
            return Err(RoomError::new(400, "room.request_invalid"));
        }
        if !valid_turn_id(turn_id) {
            return Err(RoomError::new(400, "room.request_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let Some(c) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if let Some(turn) = c.turns.iter().find(|turn| turn.turn_id == turn_id) {
            if turn.thread_id.as_deref().unwrap_or("") != thread_id
                || turn.binding_id.as_deref() != Some(binding_id)
            {
                return Err(RoomError::new(409, "room.message_conflict"));
            }
            return Ok(turn.clone());
        }
        if inner.conversations.prior(sid, turn_id).is_some() {
            return Err(RoomError::new(409, "room.input_ended"));
        }
        let bound = match (thread_id, binding_id) {
            ("", "")
                if inner
                    .browsers
                    .get(sid)
                    .is_some_and(|browser| browser.target.is_none()) =>
            {
                None
            }
            (thread, binding) => Some(
                inner
                    .browsers
                    .get(sid)
                    .and_then(|browser| browser.captured_target(thread, binding))
                    .ok_or_else(|| RoomError::new(409, "room.focus_changed"))?
                    .clone(),
            ),
        };
        if let Some(thread) = bound
            .as_ref()
            .filter(|t| !t.thread.is_empty())
            .map(|t| t.thread.clone())
        {
            if !inner.conversations.can_begin(&thread) {
                return Err(RoomError::new(429, "room.input_backlog_full"));
            }
            inner.room_for_bytes(2048)?;
        }
        let c = inner.browsers.get_mut(sid).expect("call under lock");
        // The turns already open keep their words' destination: a new one waits until one of them ends.
        if c.turns.len() >= MAX_OPEN_TURNS {
            return Err(RoomError::new(429, "room.turns_full"));
        }
        let revision = c.next_revision();
        c.turn_revision = revision;
        c.speaking = true;
        c.open_turn = Some(revision);
        let thread_id = bound
            .as_ref()
            .filter(|t| !t.thread.is_empty())
            .map(|t| t.thread.clone());
        let turn = VoiceTurn {
            session_id: sid.into(),
            turn_id: turn_id.into(),
            revision,
            epoch: 0,
            thread_id,
            binding_id: bound.as_ref().map(|t| t.binding_id.clone()),
            title: bound.as_ref().and_then(|t| t.title.clone()),
            language: c.language.clone(),
            cancelled: false,
        };
        let mut turn = turn;
        if let Some(thread) = turn.thread_id.as_deref() {
            turn.epoch = inner.advance_speech_epoch(thread, sid, turn_id);
        }
        let c = inner.browsers.get_mut(sid).expect("call under lock");
        c.turns.push_back(turn.clone());
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
        continuation: Continuation,
    ) -> Result<Value, RoomError> {
        if text.is_some_and(|text| text.len() > MAX_INPUT_BYTES) {
            return Err(RoomError::new(413, "room.text_too_long"));
        }
        let text = text.filter(|text| !text.trim().is_empty());
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let Some(browser) = inner.browsers.get(sid) else {
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
            inner.room_for_bytes_with_turn_credit(
                text.len()
                    + inner
                        .unheard
                        .next_batch_bytes(open.thread_id.as_deref().unwrap_or(""), &inner.journal),
                1,
            )?;
        }
        let browser = inner
            .browsers
            .get_mut(sid)
            .expect("the call, under the same lock");
        let turn = browser.turns.remove(index).expect("the turn found");
        // A finished merged transcript already includes earlier turns. Only an empty
        // merged cancellation transfers its obligation to a later open turn.
        let continuing = if continuation == Continuation::Merged && text.is_none() {
            browser
                .turns
                .iter()
                .rev()
                .find(|next| next.thread_id == turn.thread_id)
                .map(|next| next.epoch)
        } else {
            None
        };
        if let Some(thread) = turn.thread_id.as_deref() {
            inner.conversations.finish(thread, sid, turn_id, turn.epoch);
        }
        if browser.open_turn == Some(turn.revision) {
            browser.open_turn = browser.turns.back().map(|remaining| remaining.revision);
            browser.speaking = browser.open_turn.is_some();
        }
        let revision = turn.revision;
        if let (Some(thread), Some(next)) = (turn.thread_id.as_deref(), continuing) {
            inner.unheard.transfer_empty(thread, turn.epoch, next);
            return Ok(
                json!({"accepted":false,"revision":revision,"epoch":turn.epoch,"merged":true}),
            );
        }
        let Some(text) = text else {
            if let Some(thread) = turn.thread_id.as_deref() {
                inner.settle_empty_turn(thread, sid, turn.epoch);
            }
            return Ok(json!({"accepted":false,"revision":revision,"epoch":turn.epoch}));
        };
        if turn.cancelled {
            if let Some(thread) = turn.thread_id.as_deref() {
                inner.settle_empty_turn(thread, sid, turn.epoch);
            }
            return Ok(json!({"accepted":false,"revision":revision,"epoch":turn.epoch}));
        }
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
    pub fn cancel_input(
        &self,
        sid: &str,
        turn_id: &str,
        continuation: Continuation,
    ) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        let Some(index) = browser
            .turns
            .iter()
            .position(|turn| turn.turn_id == turn_id)
        else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        let turn = browser.turns.remove(index).expect("turn found");
        let continuing = if continuation == Continuation::Merged {
            browser
                .turns
                .iter()
                .rev()
                .find(|next| next.thread_id == turn.thread_id)
                .map(|next| next.epoch)
        } else {
            None
        };
        let thread = turn.thread_id.clone();
        if browser.open_turn == Some(turn.revision) {
            browser.open_turn = browser.turns.back().map(|remaining| remaining.revision);
            browser.speaking = browser.open_turn.is_some();
        }
        browser.notify(json!({"type":"voice-user-turn","data":{"session_id":sid,
            "phase":"cancelled","turn_id":turn_id,"thread_id":thread}}));
        if let Some(thread) = turn.thread_id.as_deref() {
            inner.conversations.finish(thread, sid, turn_id, turn.epoch);
            if let Some(next) = continuing {
                inner.unheard.transfer_empty(thread, turn.epoch, next);
            } else {
                inner.settle_empty_turn(thread, sid, turn.epoch);
            }
        }
        Ok(json!({"status":"cancelled"}))
    }
}

impl Inner {
    /// One qualified spoken boundary, shared by live and offline turns. Numeric epochs are
    /// comparable only inside this conversation, so select rows before visiting audiences.
    pub(super) fn advance_speech_epoch(&mut self, thread: &str, sid: &str, turn_id: &str) -> u64 {
        assert!(
            self.conversations.can_begin(thread),
            "epoch context reserved before turn admission"
        );
        let epoch = self.conversations.begin(thread, sid, turn_id);
        let rows: std::collections::HashSet<String> = self
            .utterances
            .iter()
            .filter(|(_, record)| {
                record.epoch < epoch
                    && !record.requested
                    && self
                        .journal
                        .find(&record.row_id)
                        .is_some_and(|row| row.thread == thread)
            })
            .map(|(_, record)| record.row_id.clone())
            .collect();
        let sessions: std::collections::HashSet<String> = self
            .utterances
            .iter()
            .filter(|(_, record)| rows.contains(&record.row_id))
            .flat_map(|(_, record)| record.clients.keys().cloned())
            .collect();
        for listener in sessions {
            self.withdraw(&listener, "newer_turn", |record, (_, status)| {
                status == "queued" && !record.requested && rows.contains(&record.row_id)
            });
        }
        epoch
    }
}
