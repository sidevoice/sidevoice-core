//! The in-memory conversation journal: its rows in order, their bound, and the state of the
//! user input waiting in it.
use std::collections::VecDeque;

use serde_json::{json, Value};

use super::util::field;
use super::{Inner, Room};

const MAX_HISTORY: usize = 2000;
const INPUT_TTL: u64 = 600;

#[derive(Default)]
pub(super) struct Row {
    pub(super) seq: u64,
    pub(super) id: String,
    pub(super) thread: String,
    pub(super) role: &'static str,
    pub(super) text: String,
    pub(super) name: Option<String>,
    pub(super) session: String,
    pub(super) revision: u64,
    pub(super) time: u64,
    pub(super) status: String,
    pub(super) reason: Option<String>,
    pub(super) language: Option<String>,
    pub(super) offline: Option<Value>,
    pub(super) payload: Option<Value>,
    pub(super) queued_at: u64,
    pub(super) attempts: usize,
    pub(super) next_attempt: u64,
    pub(super) pull_claimed_by: Option<String>,
}
impl Row {
    fn view(&self) -> Value {
        json!({"seq": self.seq, "id": self.id, "thread": self.thread, "role": self.role,
        "text": self.text, "name": self.name, "session": self.session, "revision": self.revision,
        "time": self.time, "status": self.status, "audio_reason": self.reason, "offline": self.offline})
    }
    fn is_input(&self) -> bool {
        self.role == "user"
    }
    /// The message ID the room gave this input when it was queued.
    pub(super) fn message_id(&self) -> &str {
        self.payload.as_ref().map_or("", |p| field(p, "message_id"))
    }
    /// Pending input that has waited too long to be worth delivering.
    pub(super) fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.queued_at) >= INPUT_TTL
    }
    /// Put this input back in the queue, to be delivered at the next opportunity.
    pub(super) fn requeue(&mut self) {
        self.status = "pending".into();
        self.next_attempt = 0;
    }
    pub(super) fn input_ref(&self) -> InputRef {
        InputRef {
            session: self.session.clone(),
            id: self.id.clone(),
            thread: Some(self.thread.clone()),
            revision: self.revision,
        }
    }
}

/// Which input a receipt is about, as the call that sent it knows it.
pub(super) struct InputRef {
    pub(super) session: String,
    pub(super) id: String,
    pub(super) thread: Option<String>,
    pub(super) revision: u64,
}

#[derive(Default)]
pub(super) struct Journal {
    rows: VecDeque<Row>,
    seq: u64,
}
impl Journal {
    /// Append a row under the next sequence number, then drop the oldest settled rows beyond
    /// the bound. Returns the IDs of the rows dropped.
    fn append(&mut self, mut row: Row) -> Vec<String> {
        self.seq += 1;
        row.seq = self.seq;
        self.rows.push_back(row);
        let mut dropped = Vec::new();
        while self.rows.len() > MAX_HISTORY
            && self
                .rows
                .front()
                .is_some_and(|r| !matches!(r.status.as_str(), "pending" | "sending"))
        {
            if let Some(row) = self.rows.pop_front() {
                dropped.push(row.id);
            }
        }
        dropped
    }
    pub(super) fn find(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }
    pub(super) fn find_mut(&mut self, id: &str) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }
    pub(super) fn has_thread(&self, thread: &str) -> bool {
        self.rows.iter().any(|r| r.thread == thread)
    }
    /// A thread's input rows, oldest first.
    pub(super) fn input_mut<'a>(
        &'a mut self,
        thread: &'a str,
    ) -> impl DoubleEndedIterator<Item = &'a mut Row> {
        self.rows
            .iter_mut()
            .filter(move |r| r.is_input() && r.thread == thread)
    }
    /// Pending input whose next delivery attempt is due.
    pub(super) fn due_input(&mut self, now: u64) -> impl Iterator<Item = &mut Row> {
        self.rows
            .iter_mut()
            .filter(move |r| r.is_input() && r.status == "pending" && r.next_attempt <= now)
    }
    /// Give up on pending input that waited too long.
    pub(super) fn expire_input(&mut self, now: u64) -> Vec<InputRef> {
        self.rows
            .iter_mut()
            .filter(|r| r.is_input() && r.status == "pending" && r.expired(now))
            .map(|r| {
                r.status = "not_sent".into();
                r.reason = Some("expired".into());
                r.input_ref()
            })
            .collect()
    }
    /// Withdraw a thread's input that has not reached its agent yet.
    pub(super) fn cancel_unsent(&mut self, thread: &str, reason: &str) -> Vec<InputRef> {
        self.input_mut(thread)
            .filter(|r| matches!(r.status.as_str(), "pending" | "sending"))
            .map(|r| {
                r.status = "not_sent".into();
                r.reason = Some(reason.into());
                r.input_ref()
            })
            .collect()
    }
    /// Mark read the newest of a thread's input with this message ID, unless it already was
    /// read or was never sent.
    pub(super) fn mark_read(&mut self, thread: &str, message_id: &str) -> Option<InputRef> {
        let row = self.input_mut(thread).rev().find(|r| {
            r.payload.is_some()
                && r.message_id() == message_id
                && !matches!(r.status.as_str(), "read" | "not_sent")
        })?;
        row.status = "read".into();
        Some(row.input_ref())
    }
    /// Return to the queue the pull input `released` selects among that fetched but not yet
    /// acknowledged.
    pub(super) fn release_claims(&mut self, released: impl Fn(&Row) -> bool) -> Vec<InputRef> {
        self.rows
            .iter_mut()
            .filter(|row| row.status == "delivered" && row.pull_claimed_by.is_some())
            .filter(|row| released(row))
            .map(|row| {
                row.requeue();
                row.pull_claimed_by = None;
                row.input_ref()
            })
            .collect()
    }
}

impl Inner {
    /// Append a row to the journal; utterances of rows it drops go with them.
    pub(super) fn append_row(&mut self, row: Row) {
        let dropped = self.journal.append(row);
        self.utterances.forget_rows(&dropped);
    }
}

impl Room {
    pub fn history(&self, thread: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("room lock");
        json!({"messages":inner.journal.rows.iter().filter(|r|thread.is_none_or(|t|r.thread==t)).rev().take(1000).collect::<Vec<_>>().into_iter().rev().map(Row::view).collect::<Vec<_>>()})
    }
    pub fn reply_language(&self, row_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("room lock")
            .journal
            .find(row_id)
            .and_then(|r| r.language.clone())
    }
}
