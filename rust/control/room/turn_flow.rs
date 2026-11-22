//! The synchronous sequence for a spoken turn. Every step runs under the one `Room.inner` lock;
//! no client event or other thread can observe a partially opened or closed conversation epoch.
use serde_json::{json, Value};

use super::error::RoomError;
use super::input::MAX_INPUT_BYTES;
use super::turns::{Continuation, PreparedTurn, VoiceTurn};
use super::util::millis;
use super::Inner;

/// How a spoken turn closed, for its log line: why, and how long it was open. Never what was said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Closed {
    /// `words`, `empty`, `cancelled`, `merged` (its empty ending went on into the next turn) or `call_ended`.
    pub(super) reason: &'static str,
    pub(super) duration_ms: u64,
}

impl Closed {
    pub(super) fn of(turn: &VoiceTurn, reason: &'static str) -> Self {
        Self {
            reason,
            duration_ms: millis().saturating_sub(turn.started_ms),
        }
    }
}

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
    ) -> Result<(Value, Closed), RoomError> {
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
            return Ok((
                json!({"accepted":false,"revision":revision,"epoch":turn.epoch,"merged":true}),
                Closed::of(&turn, "merged"),
            ));
        }
        let Some(text) = text else {
            if let Some(thread) = turn.thread_id.as_deref() {
                self.turn_came_to_nothing(thread, sid, turn.epoch);
            }
            let reason = if turn.cancelled { "cancelled" } else { "empty" };
            return Ok((
                json!({"accepted":false,"revision":revision,"epoch":turn.epoch}),
                Closed::of(&turn, reason),
            ));
        };
        if turn.cancelled {
            if let Some(thread) = turn.thread_id.as_deref() {
                self.turn_came_to_nothing(thread, sid, turn.epoch);
            }
            return Ok((
                json!({"accepted":false,"revision":revision,"epoch":turn.epoch}),
                Closed::of(&turn, "cancelled"),
            ));
        }
        self.record_transcript_latency(&turn, timings);
        let queued = self.queue_turn(&turn, text)?;
        if let Some(thread) = turn.thread_id.as_deref() {
            // The turn brought words: the older replies held while it was open end now, unplayed or cut. That
            // includes any the agent published while it was open.
            self.withdraw_older_epoch_replies(thread, turn.epoch, None);
            self.conversations.brought_words(thread, turn.epoch);
        }
        Ok((queued, Closed::of(&turn, "words")))
    }

    pub(super) fn cancel_spoken_turn(
        &mut self,
        sid: &str,
        turn_id: &str,
        continuation: Continuation,
    ) -> Result<(Value, Closed), RoomError> {
        // `take_cancelled` sends the cancellation notification before closing the epoch.
        let (turn, continuing) = self.take_cancelled(sid, turn_id, continuation)?;
        let mut reason = "cancelled";
        if let Some(thread) = turn.thread_id.as_deref() {
            self.conversations.finish(thread, sid, turn_id, turn.epoch);
            if let Some(next) = continuing {
                self.conversations.came_to_nothing(thread, turn.epoch);
                self.unheard.transfer_empty(thread, turn.epoch, next);
                reason = "merged";
            } else {
                self.turn_came_to_nothing(thread, sid, turn.epoch);
            }
        }
        Ok((json!({"status":"cancelled"}), Closed::of(&turn, reason)))
    }
}

/// The longest an open turn holds a reply, by default: a reply held longer is stale even if the turn comes to nothing.
const HOLD_MAX_SECONDS: f64 = 30.0;

/// `VOICE_HOLD_MAX_SECONDS` as the core reads it, in milliseconds: unreadable or negative is the default.
pub(super) fn hold_max_ms(value: Option<&str>) -> u64 {
    let seconds = value
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .unwrap_or(HOLD_MAX_SECONDS);
    (seconds * 1000.0) as u64
}

impl Inner {
    /// The epochs of the turns open on `thread`, on every call.
    pub(super) fn open_turn_epochs(&self, thread: &str) -> Vec<u64> {
        self.browsers
            .iter()
            .flat_map(|(_, browser)| browser.turns.iter())
            .filter(|turn| turn.thread_id.as_deref() == Some(thread))
            .map(|turn| turn.epoch)
            .collect()
    }

    /// Withdraw what an open turn has held for longer than the hold allows: interrupted before the turn, or published
    /// during it. It no longer plays if the turn comes to nothing; the agent is told then that it was not heard (or with
    /// the turn's words, as for any reply they withdraw).
    pub(super) fn expire_holds(&mut self) {
        let now = millis();
        let mut open: Vec<(String, VoiceTurn)> = Vec::new();
        for (sid, browser) in self.browsers.iter() {
            for turn in &browser.turns {
                open.push((sid.clone(), turn.clone()));
            }
        }
        if open.is_empty() {
            return;
        }
        let max = hold_max_ms(std::env::var("VOICE_HOLD_MAX_SECONDS").ok().as_deref());
        for (sid, turn) in open {
            let Some(thread) = turn.thread_id.as_deref() else {
                continue;
            };
            if now.saturating_sub(turn.started_ms) < max {
                continue;
            }
            let stale: std::collections::HashSet<String> = self
                .utterances
                .iter()
                .filter(|(_, record)| {
                    record.epoch < turn.epoch
                        && !record.requested
                        && !turn.expired.contains(&record.row_id)
                        && record.clients.get(&sid).is_some_and(|(_, status)| {
                            matches!(status.as_str(), "queued" | "playing")
                        })
                        && self.journal.find(&record.row_id).is_some_and(|row| {
                            row.thread == thread
                                && now.saturating_sub(row.time.max(turn.started_ms)) >= max
                        })
                })
                .map(|(_, record)| record.row_id.clone())
                .collect();
            if stale.is_empty() {
                continue;
            }
            tracing::info!(
                turn = %crate::log_safe::ClientId(&turn.turn_id),
                replies = stale.len(),
                "an open turn held replies too long, so they are withdrawn"
            );
            self.withdraw(&sid, "newer_turn", |record, _| {
                !record.requested && stale.contains(&record.row_id)
            });
            if let Some(open) = self.browsers.get_mut(&sid).and_then(|browser| {
                browser
                    .turns
                    .iter_mut()
                    .find(|open| open.turn_id == turn.turn_id)
            }) {
                open.expired.extend(stale.iter().cloned());
            }
            self.unheard.reserve_empty(thread, turn.epoch);
        }
    }
}
