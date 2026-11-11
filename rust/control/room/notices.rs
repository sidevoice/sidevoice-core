//! Sidevoice notices use exactly the same journal rows and delivery lifecycle as user input.
use serde_json::{json, Value};

use super::journal::Row;
use super::unheard::BLOCK_BYTES;
use super::util::{id, millis, seconds};
use super::Inner;

/// A note that could not be queued yet (the room's backlog was full), tried again by the pump.
pub(super) struct PendingNotice {
    thread: String,
    session: String,
    kind: String,
    related: Option<String>,
    replacement: Option<Value>,
}
impl PendingNotice {
    pub(super) fn reserved_bytes(&self) -> usize {
        BLOCK_BYTES
            + self
                .replacement
                .as_ref()
                .map_or(0, |value| value.to_string().len())
    }
}

impl Inner {
    pub(super) fn empty_obligation(&mut self, thread: &str) {
        let epochs: Vec<u64> = self
            .browsers
            .iter()
            .flat_map(|(_, browser)| browser.turns.iter())
            .filter(|turn| turn.thread_id.as_deref() == Some(thread))
            .map(|turn| turn.epoch)
            .collect();
        for epoch in epochs {
            self.unheard.reserve_empty(thread, epoch);
        }
    }

    /// Queue a note of `kind` for `thread`, with the not-heard block of the replies published until `until` (now when
    /// none) if there is one. A `user_returned` note with nothing missed is not sent. When the room's backlog is full,
    /// the note waits and the pump tries again.
    pub(super) fn queue_unheard_note(
        &mut self,
        thread: &str,
        session: &str,
        kind: &str,
        related_message_id: Option<&str>,
        until: Option<u64>,
    ) {
        if self.room_for_bytes(BLOCK_BYTES).is_err() {
            self.remember_pending(thread, session, kind, related_message_id);
            return;
        }
        let unheard = self.take_unheard(thread, until);
        if unheard.is_none() && kind == "user_returned" {
            return;
        }
        let message_id = format!("note:{}", id());
        let revision = self.browsers.get(session).map_or(0, |b| b.revision);
        let epoch = self.conversations.epoch(thread);
        let notice = json!({"kind":kind,"related_message_id":related_message_id,
            "instruction":instruction(kind)});
        let mut payload = json!({"thread_id":thread,"channel":"note","text":"",
            "message_id":message_id,"history_id":message_id,"session_id":session,
            "revision":revision,"epoch":epoch,"notice":notice});
        if let Some(unheard) = unheard {
            payload["unheard"] = unheard;
        }
        self.append_row(Row {
            id: message_id,
            thread: thread.into(),
            role: "note",
            text: String::new(),
            session: session.into(),
            revision,
            epoch,
            time: millis(),
            status: "pending".into(),
            queued_at: seconds(),
            payload: Some(payload),
            ..Row::default()
        });
    }

    pub(super) fn materialize_due_notes(&mut self) {
        let now = seconds();
        for (thread, until) in self.unheard.due_notes(now) {
            let Some(session) = self.browsers.listener_on(&thread) else {
                continue;
            };
            if self.conversations.is_open(&thread) {
                continue;
            }
            if self.room_for_bytes(BLOCK_BYTES).is_err() {
                if let Some(note) = self.unheard.note_mut(&thread) {
                    note.due = seconds() + 1;
                }
                continue;
            }
            self.unheard.drop_note(&thread);
            self.queue_unheard_note(&thread, &session, "user_returned", None, Some(until));
        }
        let pending = self.pending_notices.len();
        for _ in 0..pending {
            let Some(note) = self.pending_notices.pop_front() else {
                break;
            };
            if let Some(replacement) = note.replacement.clone() {
                if !self.queue_replacement(
                    &note.thread,
                    &note.session,
                    note_epoch(&replacement),
                    replacement,
                ) {
                    self.pending_notices.push_back(note);
                }
            } else if self.room_for_bytes(BLOCK_BYTES).is_ok() {
                self.queue_unheard_note(
                    &note.thread,
                    &note.session,
                    &note.kind,
                    note.related.as_deref(),
                    None,
                );
            } else {
                self.pending_notices.push_back(note);
            }
        }
    }

    /// A terminally undeliverable user row cannot take its notice with it. Requeue that notice (and its block) as a
    /// correlated Sidevoice note.
    pub(super) fn salvage_input_notice(&mut self, row_id: &str) {
        let Some(row) = self.journal.find(row_id) else {
            return;
        };
        if row.role != "user" || row.notice_salvaged || row.status != "not_sent" {
            return;
        }
        let Some(original) = row
            .payload
            .as_ref()
            .filter(|payload| payload.get("notice").is_some())
        else {
            return;
        };
        let thread = row.thread.clone();
        let session = row.session.clone();
        let epoch = row.epoch;
        let related = row.message_id().to_owned();
        let mut replacement = original.clone();
        replacement["channel"] = json!("note");
        replacement["text"] = json!("");
        replacement["epoch"] = json!(epoch);
        replacement["notice"]["related_message_id"] = json!(related);
        if let Some(row) = self.journal.find_mut(row_id) {
            row.notice_salvaged = true;
        }
        if !self.queue_replacement(&thread, &session, epoch, replacement.clone()) {
            self.pending_notices.push_back(PendingNotice {
                thread,
                session,
                kind: "salvaged".into(),
                related: Some(related),
                replacement: Some(replacement),
            });
        }
    }

    fn queue_replacement(
        &mut self,
        thread: &str,
        session: &str,
        epoch: u64,
        mut payload: Value,
    ) -> bool {
        if self
            .room_for_bytes(payload.to_string().len() + 1024)
            .is_err()
        {
            return false;
        }
        let message_id = format!("note:{}", id());
        payload["message_id"] = json!(message_id);
        payload["history_id"] = json!(message_id);
        self.append_row(Row {
            id: message_id,
            thread: thread.into(),
            role: "note",
            text: String::new(),
            session: session.into(),
            revision: payload["revision"].as_u64().unwrap_or(0),
            epoch,
            time: millis(),
            status: "pending".into(),
            queued_at: seconds(),
            payload: Some(payload),
            ..Row::default()
        });
        true
    }

    fn remember_pending(&mut self, thread: &str, session: &str, kind: &str, related: Option<&str>) {
        if self.pending_notices.iter().any(|pending| {
            pending.thread == thread
                && pending.session == session
                && pending.kind == kind
                && pending.related.as_deref() == related
        }) {
            return;
        }
        self.pending_notices.push_back(PendingNotice {
            thread: thread.into(),
            session: session.into(),
            kind: kind.into(),
            related: related.map(str::to_owned),
            replacement: None,
        });
    }
}

fn note_epoch(payload: &Value) -> u64 {
    payload["epoch"].as_u64().unwrap_or(0)
}

/// What to bring the person up to date with, when they missed replies.
const CATCH_UP: &str = "Bring the person up to date with only what is still relevant from what they did not hear, briefly and in fresh words: do not replay it all, and do not say it was missed.";

pub(super) fn instruction(kind: &str) -> &'static str {
    match kind {
        "turn_empty" => "The person said nothing. Resend what you meant if it still matters.",
        "user_returned" => "The person is back. Bring them up to date with only what is still relevant from what they did not hear, briefly and in fresh words: do not replay it all, and do not say it was missed.",
        _ => CATCH_UP,
    }
}
