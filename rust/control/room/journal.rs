//! The in-memory conversation journal: its rows in order, their bound, and the state of the
//! user input waiting in it.
use std::collections::HashMap;
use std::collections::VecDeque;

use serde_json::{json, Value};

use super::util::field;
use super::{Inner, Room};

const MAX_HISTORY: usize = 2000;
/// How long input waits for a conversation that is not there, unless `VOICE_INPUT_TTL_SECONDS` says.
const INPUT_TTL: u64 = 600;

fn input_ttl() -> u64 {
    static TTL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| parse_input_ttl(std::env::var("VOICE_INPUT_TTL_SECONDS").ok().as_deref()))
}
pub(super) fn parse_input_ttl(value: Option<&str>) -> u64 {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(INPUT_TTL)
}

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
    pub(super) epoch: u64,
    /// For the words of a voice turn: the client's id for the turn.
    pub(super) turn_id: Option<String>,
    pub(super) time: u64,
    pub(super) status: String,
    pub(super) reason: Option<String>,
    /// For a reply cut while it played: how many characters of it the person heard, as the call reported.
    pub(super) heard_chars: Option<u64>,
    pub(super) globally_heard: bool,
    /// Manual replay Skip does not rewrite the original playback status.
    pub(super) globally_skipped: bool,
    pub(super) language: Option<String>,
    pub(super) offline: Option<Value>,
    pub(super) payload: Option<Value>,
    /// Immutable result of the original speech publication, separate from playback status.
    pub(super) publication: Option<Value>,
    /// Immutable admission identity, including fields not shown in history.
    pub(super) publication_context: Option<Value>,
    pub(super) queued_at: u64,
    pub(super) attempts: usize,
    pub(super) next_attempt: u64,
    pub(super) pull_claimed_by: Option<String>,
    pub(super) ever_exposed: bool,
    pub(super) corrected_heard: HashMap<String, Value>,
    pub(super) notice_salvaged: bool,
}
impl Row {
    fn view(&self) -> Value {
        json!({"seq": self.seq, "id": self.id, "thread": self.thread, "role": self.role,
        "text": self.text, "name": self.name, "session": self.session, "revision": self.revision, "epoch": self.epoch,
        "time": self.time, "status": self.status, "audio_reason": self.reason, "heard_chars": self.heard_chars,
        "globally_heard":self.globally_heard,"replayable":self.role=="assistant",
        "offline": self.offline})
    }
    fn is_input(&self) -> bool {
        self.role == "user" || self.role == "note"
    }
    /// The message ID the room gave this input when it was queued.
    pub(super) fn message_id(&self) -> &str {
        self.payload.as_ref().map_or("", |p| field(p, "message_id"))
    }
    /// Pending input that has waited too long to be worth delivering.
    pub(super) fn expired(&self, now: u64) -> bool {
        self.role != "note"
            && self
                .payload
                .as_ref()
                .is_none_or(|payload| payload.get("notice").is_none())
            && now.saturating_sub(self.queued_at) >= input_ttl()
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
            epoch: self.epoch,
            note: self.role == "note",
            turn_id: self.turn_id.clone(),
        }
    }
}

/// Which input a receipt is about, as the call that sent it knows it.
pub(super) struct InputRef {
    pub(super) session: String,
    pub(super) id: String,
    pub(super) thread: Option<String>,
    pub(super) revision: u64,
    pub(super) epoch: u64,
    pub(super) note: bool,
    pub(super) turn_id: Option<String>,
}

#[derive(Default)]
pub(super) struct Journal {
    rows: VecDeque<Row>,
    seq: u64,
}
fn protected(row: &Row) -> bool {
    matches!(row.status.as_str(), "pending" | "sending")
        || (row.status == "delivered" && row.pull_claimed_by.is_some())
        || (row.role == "assistant"
            && (matches!(row.status.as_str(), "queued" | "playing")
                || (!row.globally_heard
                    && super::unheard::unheard(&row.status, row.reason.as_deref()).is_some())))
}
impl Journal {
    pub(super) fn can_append(&self) -> bool {
        self.rows.len() < MAX_HISTORY || self.rows.iter().any(|row| !protected(row))
    }
    /// Append a row under the next sequence number, then drop the oldest settled rows beyond
    /// the bound. Returns the IDs of the rows dropped.
    fn append(&mut self, mut row: Row) -> Vec<String> {
        assert!(
            self.can_append(),
            "journal admission must reserve row capacity"
        );
        self.seq += 1;
        row.seq = self.seq;
        self.rows.push_back(row);
        let mut dropped = Vec::new();
        while self.rows.len() > MAX_HISTORY {
            if let Some(index) = self.rows.iter().position(|row| !protected(row)) {
                let row = self.rows.remove(index).expect("settled row found");
                dropped.push(row.id);
            } else {
                break;
            }
        }
        dropped
    }
    pub(super) fn find(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }
    /// The person's message whose words are `text`.
    #[cfg(test)]
    pub(super) fn find_input(&self, text: &str) -> Option<&Row> {
        self.rows
            .iter()
            .find(|row| row.role == "user" && row.text == text)
    }
    pub(super) fn find_mut(&mut self, id: &str) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }
    pub(super) fn has_thread(&self, thread: &str) -> bool {
        self.rows.iter().any(|r| r.thread == thread)
    }
    pub(super) fn assistant_hearing_facts(&self, thread: &str) -> HashMap<String, Value> {
        self.rows.iter().filter(|r| r.thread == thread && r.role == "assistant")
            .filter_map(|r| {
                let (fate, heard) = if r.globally_heard {
                    (if r.status == "skipped" || r.globally_skipped {"skipped"} else {"heard"}, Some(r.text.chars().count() as u64))
                } else {
                    ("partial", r.heard_chars)
                };
                heard.map(|heard_chars| (r.id.clone(), json!({"utterance_id":r.id.rsplit_once(":voice:").map(|(_, id)| id).unwrap_or(""),
                    "history_id":r.id,"fate":fate,"heard_chars":heard_chars})))
            }).collect()
    }
    pub(super) fn unheard_summary(&self, thread: &str) -> Option<(usize, u64)> {
        let rows: Vec<_> = self
            .rows
            .iter()
            .filter(|row| {
                row.role == "assistant"
                    && row.thread == thread
                    && !row.globally_heard
                    && super::unheard::unheard(&row.status, row.reason.as_deref()).is_some()
            })
            .collect();
        Some((rows.len(), rows.first()?.time))
    }
    pub(super) fn newer_spoken_message(&self, thread: &str, epoch: u64) -> Option<(u64, String)> {
        self.rows
            .iter()
            .find(|row| {
                row.role == "user"
                    && row.thread == thread
                    && row.turn_id.is_some()
                    && row.epoch > epoch
            })
            .map(|row| (row.epoch, row.message_id().to_owned()))
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
    /// How many messages wait to reach their agents, and the bytes of their words.
    pub(super) fn waiting_input(&self) -> (usize, usize) {
        self.rows
            .iter()
            .filter(|r| {
                r.is_input()
                    && (matches!(r.status.as_str(), "pending" | "sending")
                        || (r.status == "delivered" && r.pull_claimed_by.is_some()))
            })
            .fold((0, 0), |(count, bytes), r| {
                (
                    count + 1,
                    bytes
                        + r.payload
                            .as_ref()
                            .map_or(r.text.len(), |p| p.to_string().len()),
                )
            })
    }
    /// Unresolved push input in journal order, including rows waiting for their retry deadline.
    pub(super) fn unresolved_input(&mut self) -> impl Iterator<Item = &mut Row> {
        self.rows
            .iter_mut()
            .filter(|r| r.is_input() && matches!(r.status.as_str(), "pending" | "sending"))
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
            .filter(|r| r.role == "user" && matches!(r.status.as_str(), "pending" | "sending"))
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
        row.pull_claimed_by = None;
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
        self.unheard.forget_rows(&dropped);
    }
}

impl Room {
    pub fn history(&self, thread: Option<&str>) -> Value {
        self.history_resolving(thread, &[])
    }

    /// Resolve explicit retained row IDs independently of the recent/unheard projection.
    pub fn history_resolving(&self, thread: Option<&str>, resolve_ids: &[String]) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let rows: Vec<_> = inner
            .journal
            .rows
            .iter()
            .filter(|r| thread.is_none_or(|t| r.thread == t))
            .collect();
        let recent = rows.len().saturating_sub(1000);
        let messages: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(index, row)| {
                *index >= recent
                    || (row.role == "assistant"
                        && !row.globally_heard
                        && super::unheard::unheard(&row.status, row.reason.as_deref()).is_some())
            })
            .map(|(_, row)| row.view())
            .collect();
        if resolve_ids.is_empty() {
            return json!({"messages":messages});
        }
        let mut resolved = Vec::new();
        let mut missing_ids = Vec::new();
        for id in resolve_ids {
            if let Some(row) = rows.iter().find(|row| row.id == *id) {
                resolved.push(row.view());
            } else {
                missing_ids.push(id);
            }
        }
        json!({"messages":messages,"resolved":resolved,"missing_ids":missing_ids})
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
