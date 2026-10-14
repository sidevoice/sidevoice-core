//! Sidevoice notices use exactly the same journal rows and delivery lifecycle as user input.
use serde_json::{json, Value};

use super::journal::Row;
use super::util::{id, millis, seconds};
use super::Inner;

pub(super) struct PendingNotice {
    thread: String,
    session: String,
    kind: String,
    related: Option<String>,
    skipped: Option<Value>,
    epoch: u64,
}

impl Inner {
    pub(super) fn empty_obligation(&mut self, thread: &str) {
        let epoch = self.conversations.epoch(thread);
        self.unheard.reserve_empty(thread, epoch);
    }

    pub(super) fn settle_empty_turn(&mut self, thread: &str, session: &str, epoch: u64) {
        let obliged = self.unheard.finish_empty(thread, epoch);
        if obliged || self.unheard.has(thread) {
            self.flush_all_unheard_notes(thread, session, "turn_empty", None);
        }
    }

    pub(super) fn flush_all_unheard_notes(
        &mut self,
        thread: &str,
        session: &str,
        kind: &str,
        related_message_id: Option<&str>,
    ) {
        let first = self.unheard.count(thread);
        if first == 0 {
            let before = self.journal.waiting_input().0;
            self.flush_unheard_note(thread, session, kind, related_message_id);
            if kind == "turn_empty" && self.journal.waiting_input().0 == before {
                self.remember_pending(
                    thread,
                    session,
                    kind,
                    related_message_id,
                    None,
                    self.conversations.epoch(thread),
                );
            }
            return;
        }
        while self.unheard.has(thread) {
            let before = self.unheard.count(thread);
            self.flush_unheard_note(thread, session, kind, related_message_id);
            if self.unheard.count(thread) == before {
                if kind != "user_returned" {
                    self.remember_pending(
                        thread,
                        session,
                        kind,
                        related_message_id,
                        None,
                        self.conversations.epoch(thread),
                    );
                }
                break;
            }
        }
    }

    /// Queue complete evidence, or leave it pending if the journal is full. A later pump or
    /// pull fetch tries again with the same pending evidence.
    pub(super) fn flush_unheard_note(
        &mut self,
        thread: &str,
        session: &str,
        kind: &str,
        related_message_id: Option<&str>,
    ) {
        let has_evidence = self.unheard.has(thread);
        if !has_evidence && kind != "turn_empty" && kind != "reply_skipped" {
            return;
        }
        if self
            .room_for_bytes(self.unheard.next_batch_bytes(thread, &self.journal))
            .is_err()
        {
            return;
        }
        let unheard = self.unheard.take(thread, &self.journal);
        let message_id = format!("note:{}", id());
        let revision = self.browsers.get(session).map_or(0, |b| b.revision);
        let epoch = self.conversations.epoch(thread);
        let notice = json!({"kind":kind,"related_message_id":related_message_id,
            "instruction":instruction(kind)});
        let payload = json!({"thread_id":thread,"channel":"note","text":"",
            "message_id":message_id,"history_id":message_id,"session_id":session,
            "revision":revision,"epoch":epoch,"notice":notice,"unheard":unheard});
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
        for thread in self.unheard.due_notes(now) {
            let Some(session) = self
                .unheard
                .note_mut(&thread)
                .map(|note| note.session.clone())
            else {
                continue;
            };
            let eligible = self.browsers.get(&session).is_some_and(|browser| {
                browser.is_on(&thread) && browser.parked.is_none() && browser.speech_on
            });
            if !eligible {
                self.unheard.drop_note(&thread);
                continue;
            }
            if self.conversations.is_open(&thread) {
                continue;
            }
            if !self.unheard.has(&thread) {
                self.unheard.drop_note(&thread);
                continue;
            }
            if self
                .room_for_bytes(self.unheard.next_batch_bytes(&thread, &self.journal))
                .is_err()
            {
                continue;
            }
            self.flush_all_unheard_notes(&thread, &session, "user_returned", None);
            if self.unheard.has(&thread) {
                if let Some(note) = self.unheard.note_mut(&thread) {
                    note.due = seconds() + 1;
                }
            } else {
                self.unheard.drop_note(&thread);
            }
        }
        let pending = self.pending_notices.len();
        for _ in 0..pending {
            let Some(note) = self.pending_notices.pop_front() else {
                break;
            };
            if let Some(skipped) = note.skipped.clone() {
                if !self.queue_skip_payload(&note.thread, &note.session, note.epoch, skipped) {
                    self.pending_notices.push_back(note);
                }
            } else if self.unheard.has(&note.thread) || note.kind == "turn_empty" {
                let before = self.journal.waiting_input().0;
                self.flush_unheard_note(
                    &note.thread,
                    &note.session,
                    &note.kind,
                    note.related.as_deref(),
                );
                if self.journal.waiting_input().0 == before || self.unheard.has(&note.thread) {
                    self.pending_notices.push_back(note);
                }
            }
        }
    }

    pub(super) fn queue_skip_note(&mut self, row_id: &str, session: &str) {
        let Some(row) = self.journal.find(row_id) else {
            return;
        };
        let thread = row.thread.clone();
        let epoch = row.epoch;
        let skipped = json!({"utterance_id":row.id.rsplit_once(":voice:").map(|(_, id)| id).unwrap_or(""),
            "history_id":row.id,"session_id":row.session,"revision":row.revision,
            "epoch":epoch,"text":row.text,"fate":"skipped",
            "reason":"user_skipped","heard_chars":row.text.chars().count()});
        if !self.queue_skip_payload(&thread, session, epoch, skipped.clone()) {
            self.remember_pending(
                &thread,
                session,
                "reply_skipped",
                None,
                Some(skipped),
                epoch,
            );
        }
    }

    fn queue_skip_payload(
        &mut self,
        thread: &str,
        session: &str,
        epoch: u64,
        skipped: Value,
    ) -> bool {
        let bytes = skipped.to_string().len() + 2048;
        if self.room_for_bytes(bytes).is_err() {
            return false;
        }
        let message_id = format!("note:{}", id());
        let revision = self.browsers.get(session).map_or(0, |b| b.revision);
        let payload = json!({"thread_id":thread,"channel":"note","text":"",
            "message_id":message_id,"history_id":message_id,"session_id":session,
            "revision":revision,"epoch":epoch,
            "notice":{"kind":"reply_skipped","instruction":instruction("reply_skipped"),"skipped":[skipped]}});
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
        true
    }

    fn remember_pending(
        &mut self,
        thread: &str,
        session: &str,
        kind: &str,
        related: Option<&str>,
        skipped: Option<Value>,
        epoch: u64,
    ) {
        if self.pending_notices.iter().any(|pending| {
            pending.thread == thread
                && pending.session == session
                && pending.kind == kind
                && pending.related.as_deref() == related
                && (skipped.is_none() || pending.skipped == skipped)
        }) {
            return;
        }
        self.pending_notices.push_back(PendingNotice {
            thread: thread.into(),
            session: session.into(),
            kind: kind.into(),
            related: related.map(str::to_owned),
            skipped,
            epoch,
        });
    }
}

pub(super) fn instruction(kind: &str) -> &'static str {
    match kind {
        "turn_empty" => "The person said nothing. Resend what you meant if it still matters.",
        "user_returned" => "The person is back. These replies were not heard. Send a fresh response if still relevant.",
        "reply_skipped" => "The person chose to skip this reply. Do not repeat it just because it was skipped.",
        "reply_interrupted" => "The reply was interrupted. Use the heard extent and respond to the person's current message.",
        _ => "This reply was not played. Use the current context and resend what still matters.",
    }
}
