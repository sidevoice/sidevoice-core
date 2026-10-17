//! User input entering the journal: typed text, completed voice turns, and turns spoken offline.
use serde_json::{json, Value};

use super::error::RoomError;
use super::journal::{InputRef, Row};
use super::latency::{latency_now_micros, LatencyEvent};
use super::turns::{valid_turn_id, VoiceTurn};
use super::util::valid_thread;
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
    epoch: u64,
    thread_id: &'a str,
    binding_id: &'a str,
    title: Option<String>,
    language: &'a str,
    message_id: &'a str,
    /// The client's id for the turn the words were spoken in; none for typed text.
    turn_id: Option<&'a str>,
    offline: Option<&'a str>,
    time: Option<u64>,
    playback_at_turn_start: Option<Value>,
    not_played_at_turn_start: Option<Value>,
    not_played_at_turn_start_truncated: bool,
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
        let epoch = inner.conversations.epoch(thread);
        let language = c.language.clone();
        let title = c.target.as_ref().and_then(|t| t.title.clone());
        inner.queue_input(InputDraft {
            row_id,
            text,
            session_id: sid,
            revision,
            epoch,
            thread_id: thread,
            binding_id: bid,
            title,
            language: &language,
            message_id,
            turn_id: None,
            offline: None,
            time: None,
            playback_at_turn_start: None,
            not_played_at_turn_start: None,
            not_played_at_turn_start_truncated: false,
        })
    }
    /// Commit a completed transcript to the same memory journal/outbox as typed input.
    /// The captured focus may differ from today's selection after a mid-turn switch.
    pub fn queue_voice_input(&self, turn: &VoiceTurn, text: &str) -> Result<Value, RoomError> {
        self.inner.lock().expect("room lock").queue_turn(turn, text)
    }
    /// Convenience entry for a caller that captures the selected target at this boundary.
    /// The wire path uses `offline_input_captured` with the target captured when speech began.
    #[cfg(test)]
    pub fn offline_input(
        &self,
        sid: &str,
        turn_id: &str,
        text: &str,
        time: Option<u64>,
    ) -> Result<Value, RoomError> {
        let target = {
            let inner = self.inner.lock().expect("room lock");
            inner
                .browsers
                .get(sid)
                .and_then(|browser| browser.bound_target())
                .cloned()
        };
        let Some(target) = target else {
            return Err(RoomError::new(409, "room.focus_changed"));
        };
        self.offline_input_captured(sid, turn_id, text, time, &target.thread, &target.binding_id)
    }

    /// Offline words carry the conversation captured when speech began, even if focus changed
    /// before the room accepted them. The epoch is assigned once at acceptance.
    pub fn offline_input_captured(
        &self,
        sid: &str,
        turn_id: &str,
        text: &str,
        time: Option<u64>,
        thread: &str,
        binding: &str,
    ) -> Result<Value, RoomError> {
        self.offline_input_captured_with_evidence(
            sid,
            turn_id,
            text,
            time,
            thread,
            binding,
            &Value::Null,
            &Value::Null,
            false,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "offline turn and onset evidence are captured together"
    )]
    pub fn offline_input_captured_with_evidence(
        &self,
        sid: &str,
        turn_id: &str,
        text: &str,
        time: Option<u64>,
        thread: &str,
        binding: &str,
        snapshot: &Value,
        not_played: &Value,
        truncated: bool,
    ) -> Result<Value, RoomError> {
        if !valid_turn_id(turn_id) {
            return Err(RoomError::new(400, "room.request_invalid"));
        }
        if !valid_thread(thread) || binding.is_empty() {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        words(text)?;
        let open = {
            let inner = self.inner.lock().expect("room lock");
            inner
                .browsers
                .get(sid)
                .and_then(|browser| browser.turns.iter().find(|turn| turn.turn_id == turn_id))
                .cloned()
        };
        if let Some(open) = open {
            if open.thread_id.as_deref() != Some(thread)
                || open.binding_id.as_deref() != Some(binding)
            {
                return Err(RoomError::new(409, "room.message_conflict"));
            }
            return self.finish_turn_with_evidence(
                sid,
                turn_id,
                Some(text),
                &Value::Null,
                super::Continuation::Standalone,
                snapshot,
                not_played,
                truncated,
            );
        }
        let row_id = turn_row(sid, turn_id);
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(row) = inner.journal.find(&row_id) {
            return repeated(row, text, row.thread == thread);
        }
        if inner.conversations.prior(sid, turn_id).is_some() {
            return Err(RoomError::new(409, "room.input_ended"));
        }
        if inner
            .browsers
            .get(sid)
            .is_some_and(|browser| browser.turns.iter().any(|turn| turn.turn_id == turn_id))
        {
            return Err(RoomError::new(409, "room.input_ended"));
        }
        if !inner.conversations.can_begin(thread) {
            return Err(RoomError::new(429, "room.input_backlog_full"));
        }
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let language = browser.language.clone();
        let title = inner
            .bindings
            .newest_active(thread)
            .and_then(|binding| binding.title.clone());
        inner
            .room_for_bytes(text.len() + inner.unheard.next_batch_bytes(thread, &inner.journal))?;
        let browser = inner
            .browsers
            .get_mut(sid)
            .expect("the call, under the same lock");
        let revision = browser.next_revision();
        browser.turn_revision = revision;
        let epoch = inner.open_speech_epoch(thread, sid, turn_id);
        inner.conversations.finish(thread, sid, turn_id, epoch);
        inner.revoke_pending_contract(thread, "new_words");
        let message_id = id();
        let (certified, clipped) = super::voice_contract::validated_not_played(not_played);
        inner.queue_input(InputDraft {
            row_id,
            text,
            session_id: sid,
            revision,
            epoch,
            thread_id: thread,
            binding_id: binding,
            title,
            language: &language,
            message_id: &message_id,
            turn_id: Some(turn_id),
            offline: Some("offline"),
            time,
            playback_at_turn_start: Some(super::voice_contract::validated_snapshot(snapshot)),
            not_played_at_turn_start: Some(certified),
            not_played_at_turn_start_truncated: truncated || clipped,
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
        Ok(
            json!({"accepted":true,"id":row.id,"revision":row.revision,"epoch":row.epoch,"thread_id":row.thread}),
        )
    } else {
        Err(RoomError::new(409, "room.message_conflict"))
    }
}

impl Inner {
    /// Mark transcript arrival and keep the call's measured durations before queueing its words.
    pub(super) fn record_transcript_latency(&mut self, turn: &VoiceTurn, timings: &Value) {
        let Some(thread) = turn.thread_id.as_deref() else {
            return;
        };
        self.mark_latency(
            &turn.session_id,
            thread,
            turn.revision,
            None,
            LatencyEvent::Transcript,
            latency_now_micros(),
        );
        if let Some(timings) = timings.as_object() {
            for (name, milliseconds) in timings {
                if let Some(milliseconds) = milliseconds.as_f64() {
                    self.record_latency_duration(
                        &turn.session_id,
                        thread,
                        turn.revision,
                        None,
                        name,
                        milliseconds,
                    );
                }
            }
        }
    }

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
            epoch: turn.epoch,
            thread_id: thread,
            binding_id: bid,
            title: turn.title.clone(),
            language: &turn.language,
            message_id: &message_id,
            turn_id: Some(&turn.turn_id),
            offline: None,
            time: None,
            playback_at_turn_start: Some(turn.playback_at_turn_start.clone()),
            not_played_at_turn_start: Some(turn.not_played_at_turn_start.clone()),
            not_played_at_turn_start_truncated: turn.not_played_at_turn_start_truncated,
        })
    }
    fn queue_input(&mut self, draft: InputDraft<'_>) -> Result<Value, RoomError> {
        self.room_for(draft.text)?;
        if draft.turn_id.is_some() {
            self.room_for_bytes(
                draft.text.len()
                    + self
                        .unheard
                        .next_batch_bytes(draft.thread_id, &self.journal),
            )?;
        }
        let InputDraft {
            row_id,
            text,
            session_id,
            revision,
            epoch,
            thread_id,
            binding_id,
            title,
            language,
            message_id,
            turn_id,
            offline,
            time,
            playback_at_turn_start,
            not_played_at_turn_start,
            not_played_at_turn_start_truncated,
        } = draft;
        if turn_id.is_none() {
            self.revoke_pending_contract(thread_id, "new_words");
            self.revoke_queued_contract(thread_id, "new_words");
        }
        let mut payload = json!({"thread_id":thread_id,"text":text,"message_id":message_id,"session_id":session_id,
            "history_id":row_id,"revision":revision,"epoch":epoch,"channel":"voice","binding_id":binding_id,"title":title,
            "origin":if turn_id.is_some() {"spoken_room"} else {"typed_room"}});
        if let Some(snapshot) = playback_at_turn_start {
            payload["playback_at_turn_start"] =
                self.verified_snapshot(thread_id, session_id, &snapshot);
        }
        if let Some(not_played) = not_played_at_turn_start {
            payload["not_played_at_turn_start"] =
                self.verified_not_played(thread_id, session_id, &not_played);
            payload["not_played_at_turn_start_truncated"] =
                json!(not_played_at_turn_start_truncated);
        }
        self.contract_input_facts(thread_id, &mut payload);
        // Typed context does not consume a spoken turn's pending playback evidence.
        if turn_id.is_some() {
            self.unheard.resolve_words(thread_id);
            if let Some(unheard) = self.unheard.take(thread_id, &self.journal) {
                let kind = "reply_not_heard";
                payload["notice"] =
                    json!({"kind":kind,"instruction":super::notices::instruction(kind)});
                payload["unheard"] = unheard;
            }
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
            epoch,
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
        if turn_id.is_some() && self.unheard.has(thread_id) {
            self.flush_all_unheard_notes(
                thread_id,
                session_id,
                "reply_not_heard",
                Some(message_id),
            );
        }
        self.mark_latency(
            session_id,
            thread_id,
            revision,
            None,
            LatencyEvent::Queued,
            latency_now_micros(),
        );
        self.browsers.input_receipt(&input, "pending");
        Ok(
            json!({"accepted":true,"id":input.id,"revision":revision,"epoch":epoch,"thread_id":thread_id}),
        )
    }
    /// Refuses `text` when the messages already waiting to reach their agents are as many, or as long, as the room
    /// keeps: a message is only taken when it can wait for its agent.
    pub(super) fn room_for(&self, text: &str) -> Result<(), RoomError> {
        self.room_for_bytes(text.len() + 1024)
    }
    pub(super) fn room_for_bytes(&self, bytes_needed: usize) -> Result<(), RoomError> {
        self.room_for_bytes_with_turn_credit(bytes_needed, 0)
    }
    pub(super) fn room_for_bytes_with_turn_credit(
        &self,
        bytes_needed: usize,
        turn_credit: usize,
    ) -> Result<(), RoomError> {
        let (journal_count, journal_bytes) = self.journal.waiting_input();
        let reserved_turns = self.conversations.open_count().saturating_sub(turn_credit);
        let count = journal_count + self.pending_notices.len() + reserved_turns;
        let bytes = journal_bytes
            + self
                .pending_notices
                .iter()
                .map(|notice| notice.reserved_bytes())
                .sum::<usize>()
            + reserved_turns * (MAX_INPUT_BYTES + 1024);
        if !self.journal.can_append()
            || count >= MAX_WAITING_INPUT
            || bytes + bytes_needed > MAX_WAITING_BYTES
        {
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
            epoch: 0,
            note: false,
            turn_id: Some(turn_id.to_owned()),
        };
        self.browsers.input_receipt(&input, "not_sent");
        json!({"accepted":false,"id":input.id,"revision":revision,"status":"not_sent"})
    }
}
