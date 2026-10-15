//! Confirmed partial and not-heard replies, per conversation, until the agent is told.
//! Physical playback status and stop cause do not decide semantic hearing.
//!
//! Nothing here is ever played. Full reply evidence goes to the agent in batches with the
//! ending spoken message or through ordinary note rows after [`NOTE_GRACE_SECONDS`].
use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{json, Value};

use super::journal::Journal;
use super::util::seconds;
use super::Inner;

/// Maximum full replies in one evidence envelope.
pub(super) const TOLD_REPLIES: usize = 8;
/// Room's maximum retained original replies.
const KEPT_PER_THREAD: usize = 2048;
/// How long a person who came back may stay silent before the agent is sent a note.
pub(super) const NOTE_GRACE_SECONDS: u64 = 5;

/// One confirmed miss and its maximum heard prefix.
struct Missed {
    row_id: String,
    heard_chars: Option<usize>,
}

#[derive(Default)]
struct Pending {
    /// Inserted when each reply becomes unheard; publication order is recovered from the journal.
    rows: VecDeque<Missed>,
}

/// A note waiting to tell an agent what its returning person did not hear.
pub(super) struct Note {
    pub(super) due: u64,
}

#[derive(Default)]
pub(super) struct Unheard {
    by_thread: HashMap<String, Pending>,
    queued_rows: HashSet<String>,
    notes: HashMap<String, Note>,
    empty_obligations: HashSet<(String, u64)>,
}

impl Unheard {
    pub(super) fn has_queued(&self, row_id: &str) -> bool {
        self.queued_rows.contains(row_id)
    }

    pub(super) fn forget_rows(&mut self, rows: &[String]) {
        for id in rows {
            self.queued_rows.remove(id);
        }
        for pending in self.by_thread.values_mut() {
            pending.rows.retain(|missed| !rows.contains(&missed.row_id));
        }
        self.by_thread.retain(|_, pending| !pending.rows.is_empty());
    }
    /// A confirmed miss is present when `unheard` is true; false removes it.
    pub(super) fn update(
        &mut self,
        thread: &str,
        row_id: &str,
        unheard: bool,
        heard_chars: Option<usize>,
    ) {
        match unheard {
            true => {
                let pending = self.by_thread.entry(thread.to_owned()).or_default();
                if let Some(entry) = pending.rows.iter_mut().find(|m| m.row_id == row_id) {
                    entry.heard_chars = match (entry.heard_chars, heard_chars) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                    return;
                }
                pending.rows.push_back(Missed {
                    row_id: row_id.to_owned(),
                    heard_chars,
                });
                // Admission is bounded by the journal/utterance limits. Never discard evidence.
                debug_assert!(pending.rows.len() <= KEPT_PER_THREAD);
            }
            false => {
                if let Some(pending) = self.by_thread.get_mut(thread) {
                    pending.rows.retain(|m| m.row_id != row_id);
                    if pending.rows.is_empty() {
                        self.by_thread.remove(thread);
                    }
                }
            }
        }
    }

    pub(super) fn has(&self, thread: &str) -> bool {
        self.by_thread
            .get(thread)
            .is_some_and(|pending| !pending.rows.is_empty())
    }

    /// Take up to [`TOLD_REPLIES`] full replies in publication order. Remaining evidence stays
    /// pending for correlated follow-up notice rows.
    pub(super) fn take(&mut self, thread: &str, journal: &Journal) -> Option<Value> {
        let pending = self.by_thread.get_mut(thread)?;
        let mut ordered: Vec<_> = pending.rows.drain(..).collect();
        ordered.sort_by_key(|missed| journal.find(&missed.row_id).map_or(u64::MAX, |row| row.seq));
        let remaining = ordered.split_off(ordered.len().min(TOLD_REPLIES));
        pending.rows = remaining.into();
        let batch = ordered;
        let count = batch.len();
        if count == 0 {
            return None;
        }
        for missed in &batch {
            self.queued_rows.insert(missed.row_id.clone());
        }
        let rows: Vec<_> = batch
            .iter()
            .filter_map(|missed| Some((journal.find(&missed.row_id)?, missed)))
            .collect();
        let replies: Vec<Value> = rows
            .iter()
            .map(|(row, missed)| {
                let mut reply = json!({"utterance_id":row.id.rsplit_once(":voice:").map(|(_, id)| id).unwrap_or(""),
                    "history_id":row.id,"session_id":row.session,"revision":row.revision,
                    "epoch":row.epoch,"text":row.text,"fate":row.hearing,
                    "heard_chars":Value::Null});
                if let Some(heard) = missed.heard_chars {
                    reply["heard_chars"] = json!(heard);
                }
                reply
            })
            .collect();
        Some(json!({"count":count,"replies":replies}))
    }

    /// A person came back to `thread` in call `session`: if they missed something, the agent is
    /// told unless they speak first.
    pub(super) fn offer_note(&mut self, thread: &str, _session: &str) {
        if self.notes.contains_key(thread) {
            return;
        }
        self.notes.insert(
            thread.to_owned(),
            Note {
                due: seconds() + NOTE_GRACE_SECONDS,
            },
        );
    }

    /// The notes due at `now`, by conversation.
    pub(super) fn due_notes(&self, now: u64) -> Vec<String> {
        self.notes
            .iter()
            .filter(|(_, note)| note.due <= now)
            .map(|(thread, _)| thread.clone())
            .collect()
    }

    pub(super) fn note_mut(&mut self, thread: &str) -> Option<&mut Note> {
        self.notes.get_mut(thread)
    }

    pub(super) fn drop_note(&mut self, thread: &str) {
        self.notes.remove(thread);
    }

    pub(super) fn count(&self, thread: &str) -> usize {
        self.by_thread.get(thread).map_or(0, |p| p.rows.len())
    }

    /// Bytes already owed to an agent but not yet placed in an input envelope.
    pub(super) fn pending_bytes(&self, journal: &Journal) -> usize {
        self.by_thread
            .values()
            .flat_map(|pending| pending.rows.iter())
            .filter_map(|missed| journal.find(&missed.row_id))
            .map(|row| row.text.len() + 512)
            .sum()
    }

    pub(super) fn next_batch_bytes(&self, thread: &str, journal: &Journal) -> usize {
        let mut rows: Vec<_> = self
            .by_thread
            .get(thread)
            .into_iter()
            .flat_map(|pending| pending.rows.iter())
            .filter_map(|missed| journal.find(&missed.row_id))
            .collect();
        rows.sort_by_key(|row| row.seq);
        2048 + rows
            .into_iter()
            .take(TOLD_REPLIES)
            .map(|row| {
                serde_json::to_string(&row.text).map_or(row.text.len() * 6, |text| text.len()) + 512
            })
            .sum::<usize>()
    }

    pub(super) fn reserve_empty(&mut self, thread: &str, epoch: u64) {
        self.empty_obligations.insert((thread.to_owned(), epoch));
    }

    pub(super) fn finish_empty(&mut self, thread: &str, epoch: u64) -> bool {
        let removed = self.empty_obligations.remove(&(thread.to_owned(), epoch));
        removed
            && !self
                .empty_obligations
                .iter()
                .any(|(pending, _)| pending == thread)
    }

    pub(super) fn resolve_words(&mut self, thread: &str) {
        self.empty_obligations
            .retain(|(pending, _)| pending != thread);
        self.notes.remove(thread);
    }

    pub(super) fn transfer_empty(&mut self, thread: &str, from: u64, to: u64) {
        if self.empty_obligations.remove(&(thread.to_owned(), from)) {
            self.reserve_empty(thread, to);
        }
    }
}

impl Inner {
    /// Record an empty end before flushing any notice owed to the agent.
    pub(super) fn settle_empty_turn(&mut self, thread: &str, session: &str, epoch: u64) {
        self.conversations.record_empty(thread, epoch, session);
        let obliged = self.unheard.finish_empty(thread, epoch);
        if obliged || self.unheard.has(thread) {
            self.flush_all_unheard_notes(thread, session, "turn_empty", None);
        }
    }

    /// Keep a reply's place on its conversation's list after its row changed.
    pub(super) fn track_unheard(&mut self, row_id: &str) {
        let Some(row) = self
            .journal
            .find(row_id)
            .filter(|row| row.role == "assistant")
        else {
            return;
        };
        let verdict = matches!(row.hearing.as_deref(), Some("partial" | "not_heard"));
        if row.hearing.is_some() {
            let thread = row.thread.clone();
            let heard_chars = row
                .heard_chars
                .and_then(|heard| usize::try_from(heard).ok());
            if !verdict || !self.unheard.has_queued(row_id) {
                self.unheard.update(&thread, row_id, verdict, heard_chars);
            }
        }
    }

    /// Call `sid` is back on its conversation: its agent is told what was missed there, unless
    /// the person speaks first.
    pub(super) fn offer_note(&mut self, sid: &str) {
        if let Some(thread) = self
            .browsers
            .get(sid)
            .and_then(|browser| browser.bound_target())
            .map(|target| target.thread.clone())
        {
            self.unheard.offer_note(&thread, sid);
        }
    }
}
