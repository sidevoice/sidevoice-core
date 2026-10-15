//! The call's spoken turns and the conversation captured for each one. Epochs, playback,
//! hearing, and input delivery are sequenced by `turn_flow` under the room lock.
use serde_json::{json, Value};

use super::browsers::MAX_OPEN_TURNS;
use super::error::RoomError;
use super::{Inner, Room};

/// The longest turn id the room takes, in bytes.
const MAX_TURN_ID: usize = 64;

/// A turn as the room took it when it started: the client's id, revision, and captured focus.
#[derive(Clone, Debug)]
pub struct VoiceTurn {
    pub session_id: String,
    pub turn_id: String,
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

/// A duplicate start returns its existing turn without opening another epoch.
pub(super) enum PreparedTurn {
    Existing(VoiceTurn),
    New(VoiceTurn),
}

impl Room {
    /// Begin a turn at the conversation and binding captured when its speech started.
    pub fn begin_turn(
        &self,
        sid: &str,
        turn_id: &str,
        thread_id: &str,
        binding_id: &str,
    ) -> Result<VoiceTurn, RoomError> {
        self.inner
            .lock()
            .expect("room lock")
            .begin_spoken_turn(sid, turn_id, thread_id, binding_id)
    }

    /// Finish a turn with its transcript, or with none when it was empty or cancelled.
    pub fn finish_turn(
        &self,
        sid: &str,
        turn_id: &str,
        text: Option<&str>,
        timings: &Value,
        continuation: Continuation,
    ) -> Result<Value, RoomError> {
        self.inner.lock().expect("room lock").finish_spoken_turn(
            sid,
            turn_id,
            text,
            timings,
            continuation,
        )
    }

    /// Cancel a spoken turn; its eventual words are dropped.
    pub fn cancel_turn(
        &self,
        sid: &str,
        turn_id: &str,
        continuation: Continuation,
    ) -> Result<Value, RoomError> {
        self.inner
            .lock()
            .expect("room lock")
            .cancel_spoken_turn(sid, turn_id, continuation)
    }
}

impl Inner {
    /// Resolve the captured target and reserve a place before the coordinator opens its epoch.
    pub(super) fn prepare_begin(
        &mut self,
        sid: &str,
        turn_id: &str,
        thread_id: &str,
        binding_id: &str,
    ) -> Result<PreparedTurn, RoomError> {
        if binding_id.is_empty() && !thread_id.is_empty() {
            return Err(RoomError::new(400, "room.request_invalid"));
        }
        if !valid_turn_id(turn_id) {
            return Err(RoomError::new(400, "room.request_invalid"));
        }
        let Some(browser) = self.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if let Some(turn) = browser.turns.iter().find(|turn| turn.turn_id == turn_id) {
            if turn.thread_id.as_deref().unwrap_or("") != thread_id
                || turn.binding_id.as_deref() != Some(binding_id)
            {
                return Err(RoomError::new(409, "room.message_conflict"));
            }
            return Ok(PreparedTurn::Existing(turn.clone()));
        }
        if self.conversations.prior(sid, turn_id).is_some() {
            return Err(RoomError::new(409, "room.input_ended"));
        }
        let bound = match (thread_id, binding_id) {
            ("", "")
                if self
                    .browsers
                    .get(sid)
                    .is_some_and(|browser| browser.target.is_none()) =>
            {
                None
            }
            (thread, binding) => Some(
                self.browsers
                    .get(sid)
                    .and_then(|browser| browser.captured_target(thread, binding))
                    .ok_or_else(|| RoomError::new(409, "room.focus_changed"))?
                    .clone(),
            ),
        };
        if let Some(thread) = bound
            .as_ref()
            .filter(|target| !target.thread.is_empty())
            .map(|target| target.thread.clone())
        {
            if !self.conversations.can_begin(&thread) {
                return Err(RoomError::new(429, "room.input_backlog_full"));
            }
            self.room_for_bytes(2048)?;
        }
        let browser = self.browsers.get_mut(sid).expect("call under lock");
        if browser.turns.len() >= MAX_OPEN_TURNS {
            return Err(RoomError::new(429, "room.turns_full"));
        }
        let revision = browser.next_revision();
        browser.turn_revision = revision;
        browser.speaking = true;
        browser.open_turn = Some(revision);
        let thread_id = bound
            .as_ref()
            .filter(|target| !target.thread.is_empty())
            .map(|target| target.thread.clone());
        Ok(PreparedTurn::New(VoiceTurn {
            session_id: sid.into(),
            turn_id: turn_id.into(),
            revision,
            epoch: 0,
            thread_id,
            binding_id: bound.as_ref().map(|target| target.binding_id.clone()),
            title: bound.as_ref().and_then(|target| target.title.clone()),
            language: browser.language.clone(),
            cancelled: false,
        }))
    }

    /// Keep the started turn only after the new epoch has withdrawn older queued replies.
    pub(super) fn retain_started(&mut self, turn: VoiceTurn) {
        self.browsers
            .get_mut(&turn.session_id)
            .expect("call under lock")
            .turns
            .push_back(turn);
    }

    /// Remove a finished turn and find a same-conversation successor for an empty merge.
    pub(super) fn take_finished(
        &mut self,
        sid: &str,
        turn_id: &str,
        continuation: Continuation,
    ) -> Result<(VoiceTurn, Option<u64>), RoomError> {
        let Some(browser) = self.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let Some(index) = browser
            .turns
            .iter()
            .position(|turn| turn.turn_id == turn_id)
        else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        let turn = browser.turns.remove(index).expect("the turn found");
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
        Ok((turn, continuing))
    }

    /// Update the call after its conversation's open turn has been closed.
    pub(super) fn finish_browser_turn(&mut self, sid: &str, turn: &VoiceTurn) {
        let browser = self.browsers.get_mut(sid).expect("call under lock");
        if browser.open_turn == Some(turn.revision) {
            browser.open_turn = browser.turns.back().map(|remaining| remaining.revision);
            browser.speaking = browser.open_turn.is_some();
        }
    }

    /// Remove a cancelled turn, update call state, and emit the existing cancellation event.
    pub(super) fn take_cancelled(
        &mut self,
        sid: &str,
        turn_id: &str,
        continuation: Continuation,
    ) -> Result<(VoiceTurn, Option<u64>), RoomError> {
        let Some(browser) = self.browsers.get_mut(sid) else {
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
        if browser.open_turn == Some(turn.revision) {
            browser.open_turn = browser.turns.back().map(|remaining| remaining.revision);
            browser.speaking = browser.open_turn.is_some();
        }
        browser.notify(json!({"type":"voice-user-turn","data":{"session_id":sid,
            "phase":"cancelled","turn_id":turn_id,"thread_id":turn.thread_id}}));
        Ok((turn, continuing))
    }
}
