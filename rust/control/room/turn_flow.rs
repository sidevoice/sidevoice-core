//! The synchronous sequence for a spoken turn. Every step runs under the one `Room.inner` lock;
//! no client event or other thread can observe a partially opened or closed conversation epoch.
use serde_json::{json, Value};

use super::error::RoomError;
use super::input::MAX_INPUT_BYTES;
use super::turns::{Continuation, PreparedTurn, VoiceTurn};
use super::Inner;

impl Inner {
    pub(super) fn begin_spoken_turn(
        &mut self,
        sid: &str,
        turn_id: &str,
        thread_id: &str,
        binding_id: &str,
    ) -> Result<VoiceTurn, RoomError> {
        let mut turn = match self.prepare_begin(sid, turn_id, thread_id, binding_id)? {
            PreparedTurn::Existing(turn) => return Ok(turn),
            PreparedTurn::New(turn) => turn,
        };
        if let Some(thread) = turn.thread_id.as_deref() {
            turn.epoch = self.advance_speech_epoch(thread, sid, turn_id);
            // The call the person speaks into holds what it has of older replies while the turn is open: they end
            // only if it brings words, and play on if it comes to nothing. Other calls on the conversation are not
            // holding them, so they are withdrawn there.
            self.withdraw_older_epoch_replies(thread, turn.epoch, Some(sid));
        }
        self.retain_started(turn.clone());
        Ok(turn)
    }

    /// Open the conversation epoch of offline voice input, which comes with its words: older replies are withdrawn.
    pub(super) fn open_speech_epoch(&mut self, thread: &str, sid: &str, turn_id: &str) -> u64 {
        let epoch = self.advance_speech_epoch(thread, sid, turn_id);
        self.withdraw_older_epoch_replies(thread, epoch, None);
        epoch
    }

    /// A spoken turn came to nothing: the replies it held play on, and the agent's next reply is not late for it.
    fn turn_came_to_nothing(&mut self, thread: &str, sid: &str, epoch: u64) {
        self.conversations.came_to_nothing(thread, epoch);
        self.settle_empty_turn(thread, sid, epoch);
    }

    pub(super) fn finish_spoken_turn(
        &mut self,
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
        let Some(browser) = self.browsers.get(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let Some(open) = browser.turns.iter().find(|turn| turn.turn_id == turn_id) else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        // Keep a retryable refusal from consuming the turn or its captured destination.
        if let Some(text) = text.filter(|_| !open.cancelled && open.thread_id.is_some()) {
            self.room_for_bytes_with_turn_credit(text.len() + super::unheard::BLOCK_BYTES, 1)?;
        }
        // Nonempty merged text already contains earlier words; only empty endings transfer.
        let effective = if text.is_none() {
            continuation
        } else {
            Continuation::Standalone
        };
        let (turn, continuing) = self.take_finished(sid, turn_id, effective)?;
        if let Some(thread) = turn.thread_id.as_deref() {
            self.conversations.finish(thread, sid, turn_id, turn.epoch);
        }
        self.finish_browser_turn(sid, &turn);
        let revision = turn.revision;
        if let (Some(thread), Some(next)) = (turn.thread_id.as_deref(), continuing) {
            self.conversations.came_to_nothing(thread, turn.epoch);
            self.unheard.transfer_empty(thread, turn.epoch, next);
            return Ok(
                json!({"accepted":false,"revision":revision,"epoch":turn.epoch,"merged":true}),
            );
        }
        let Some(text) = text else {
            if let Some(thread) = turn.thread_id.as_deref() {
                self.turn_came_to_nothing(thread, sid, turn.epoch);
            }
            return Ok(json!({"accepted":false,"revision":revision,"epoch":turn.epoch}));
        };
        if turn.cancelled {
            if let Some(thread) = turn.thread_id.as_deref() {
                self.turn_came_to_nothing(thread, sid, turn.epoch);
            }
            return Ok(json!({"accepted":false,"revision":revision,"epoch":turn.epoch}));
        }
        self.record_transcript_latency(&turn, timings);
        let queued = self.queue_turn(&turn, text)?;
        if let Some(thread) = turn.thread_id.as_deref() {
            // The turn brought words: the older replies held while it was open end now, unplayed or cut.
            self.withdraw_older_epoch_replies(thread, turn.epoch, None);
            self.conversations.brought_words(thread, turn.epoch);
        }
        Ok(queued)
    }

    pub(super) fn cancel_spoken_turn(
        &mut self,
        sid: &str,
        turn_id: &str,
        continuation: Continuation,
    ) -> Result<Value, RoomError> {
        // `take_cancelled` sends the cancellation notification before closing the epoch.
        let (turn, continuing) = self.take_cancelled(sid, turn_id, continuation)?;
        if let Some(thread) = turn.thread_id.as_deref() {
            self.conversations.finish(thread, sid, turn_id, turn.epoch);
            if let Some(next) = continuing {
                self.conversations.came_to_nothing(thread, turn.epoch);
                self.unheard.transfer_empty(thread, turn.epoch, next);
            } else {
                self.turn_came_to_nothing(thread, sid, turn.epoch);
            }
        }
        Ok(json!({"status":"cancelled"}))
    }
}
