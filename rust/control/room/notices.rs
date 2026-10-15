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
    epoch: u64,
    correction: Option<Value>,
    replacement: Option<Value>,
}
impl PendingNotice {
    pub(super) fn reserved_bytes(&self) -> usize {
        2048 + self
            .correction
            .as_ref()
            .or(self.replacement.as_ref())
            .map_or(0, |value| value.to_string().len())
    }
}

/// Corrections may only increase confirmed hearing, including after a frozen envelope or prior correction.
fn hearing_advances(next: &Value, previous: &Value) -> bool {
    let next_fate = next["fate"].as_str().unwrap_or("partial");
    let previous_fate = previous["fate"].as_str().unwrap_or("partial");
    if previous_fate == "heard" {
        return false;
    }
    if next_fate == "heard" {
        return true;
    }
    if next_fate == "partial" && previous_fate == "not_heard" {
        return true;
    }
    let earlier = previous["heard_chars"].as_u64();
    let later = next["heard_chars"].as_u64();
    later.is_some_and(|later| earlier.is_none_or(|earlier| later > earlier))
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

    pub(super) fn settle_empty_turn(&mut self, thread: &str, session: &str, epoch: u64) {
        self.conversations.record_empty(thread, epoch, session);
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
        if !has_evidence && kind != "turn_empty" {
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
            let session = self
                .browsers
                .on_thread(&thread)
                .find(|(_, browser)| browser.parked.is_none() && browser.speech_on)
                .map(|(sid, _)| sid.to_owned());
            let Some(session) = session else {
                continue;
            };
            if self.conversations.is_open(&thread) {
                continue;
            }
            if !self.unheard.has(&thread) {
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
            if let Some(correction) = note.correction.clone() {
                if !self.queue_heard_correction(&note.thread, &note.session, note.epoch, correction)
                {
                    self.pending_notices.push_back(note);
                }
            } else if let Some(replacement) = note.replacement.clone() {
                if !self.queue_replacement(&note.thread, &note.session, note.epoch, replacement) {
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

    /// A notice that has not been offered to the agent may be rewritten when another
    /// device confirms hearing. Claimed or dispatched messages remain frozen.
    pub(super) fn reconcile_unclaimed(&mut self, thread: &str) {
        let heard = self.journal.assistant_hearing_facts(thread);
        let mut corrections = Vec::new();
        for row in self.journal.input_mut(thread) {
            if row.payload.is_none() {
                continue;
            }
            let row_message_id = row.message_id().to_owned();
            let payload = row.payload.as_mut().expect("checked");
            let Some(replies) = payload
                .pointer("/unheard/replies")
                .and_then(Value::as_array)
            else {
                continue;
            };
            let changed_hearing: Vec<_> = replies
                .iter()
                .filter_map(|reply| {
                    let id = reply["history_id"].as_str()?;
                    let fact = heard.get(id)?;
                    if !hearing_advances(fact, reply) {
                        return None;
                    }
                    Some((id.to_owned(), fact.clone()))
                })
                .collect();
            if row.status == "pending" && !row.ever_exposed {
                let replies = payload
                    .pointer_mut("/unheard/replies")
                    .and_then(Value::as_array_mut)
                    .expect("checked");
                replies.retain_mut(|reply| {
                    let Some(fact) = reply["history_id"].as_str().and_then(|id| heard.get(id))
                    else {
                        return true;
                    };
                    if fact["fate"] == "partial" {
                        reply["heard_chars"] = fact["heard_chars"].clone();
                        reply["fate"] = fact["fate"].clone();
                        true
                    } else {
                        false
                    }
                });
                let count = replies.len();
                payload["unheard"]["count"] = json!(count);
                if count == 0 {
                    payload.as_object_mut().unwrap().remove("unheard");
                    if row.role == "note" {
                        row.status = "not_sent".into();
                    } else {
                        payload.as_object_mut().unwrap().remove("notice");
                    }
                }
            } else if row.ever_exposed {
                for (id, fact) in changed_hearing {
                    let baseline = row
                        .corrected_heard
                        .get(&id)
                        .or_else(|| replies.iter().find(|reply| reply["history_id"] == id));
                    if baseline.is_none_or(|baseline| hearing_advances(&fact, baseline)) {
                        row.corrected_heard.insert(id, fact.clone());
                        corrections.push((
                            row.session.clone(),
                            row.epoch,
                            row_message_id.clone(),
                            fact,
                        ));
                    }
                }
            }
        }
        for (session, epoch, prior, fact) in corrections {
            let correction = json!({"corrects_message_id":prior,"heard":[fact]});
            if !self.queue_heard_correction(thread, &session, epoch, correction.clone()) {
                self.pending_notices.push_back(PendingNotice {
                    thread: thread.into(),
                    session,
                    kind: "reply_heard".into(),
                    related: Some(prior),
                    epoch,
                    correction: Some(correction),
                    replacement: None,
                });
            }
        }
    }

    fn queue_heard_correction(
        &mut self,
        thread: &str,
        session: &str,
        epoch: u64,
        correction: Value,
    ) -> bool {
        if self
            .room_for_bytes(correction.to_string().len() + 1024)
            .is_err()
        {
            return false;
        }
        let message_id = format!("note:{}", id());
        let revision = self.browsers.get(session).map_or(0, |b| b.revision);
        let instruction = if correction["heard"]
            .as_array()
            .is_some_and(|heard| heard.iter().any(|fact| fact["fate"] == "partial"))
        {
            "A later receipt confirmed more of this reply was heard. Use the updated prefix; the remaining text may still need a fresh response."
        } else {
            instruction("reply_heard")
        };
        let payload = json!({"thread_id":thread,"channel":"note","text":"", "message_id":message_id,
            "history_id":message_id,"session_id":session,"revision":revision,"epoch":epoch,
            "notice":{"kind":"reply_heard","instruction":instruction,
                "corrects_message_id":correction["corrects_message_id"],"heard":correction["heard"]}});
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

    /// A terminally undeliverable user row cannot take its notice evidence with it.
    /// Requeue that evidence as a correlated Sidevoice note, preserving full text and IDs.
    pub(super) fn salvage_input_notice(&mut self, row_id: &str) {
        let Some(row) = self.journal.find(row_id) else {
            return;
        };
        if row.role != "user"
            || row.notice_salvaged
            || !matches!(row.status.as_str(), "not_sent" | "unconfirmed")
        {
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
                epoch,
                correction: None,
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

    fn remember_pending(
        &mut self,
        thread: &str,
        session: &str,
        kind: &str,
        related: Option<&str>,
        epoch: u64,
    ) {
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
            epoch,
            correction: None,
            replacement: None,
        });
    }
}

pub(super) fn instruction(kind: &str) -> &'static str {
    match kind {
        "turn_empty" => "The person said nothing. Resend what you meant if it still matters.",
        "user_returned" => "The person is back. Use the confirmed hearing extents and respond again if still relevant.",
        "reply_heard" => "This reply was heard after the earlier notice. Do not resend it solely because of that notice.",
        _ => "Use the confirmed hearing extent and current context; respond again if still relevant.",
    }
}
