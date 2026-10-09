//! What the person did not hear, per conversation, until the agent is told.
//!
//! A reply counts as unheard when it ends without being played to the person:
//! - cut while it played, because the person spoke over it (`user_interrupted`). Nothing tells the
//!   room how far playback got, so the whole reply counts, flagged `cut`: its start was heard;
//! - dropped before it played: superseded by a newer message (`newer_turn`), held for a call that
//!   dropped (`unheard`), stopped by a change of conversation or the end of the call
//!   (`focus_changed`, `call_ended`, `session_changed`), refused for a full queue or failed on the
//!   page (`queue_full`, `playback_failed`);
//! - published with nobody listening on its conversation (`text_only`).
//!
//! Nothing here is ever played. The list goes to the agent once, bounded ([`Told`]): with the
//! next message on the conversation, or as a note when the person comes back to it and says
//! nothing for [`NOTE_GRACE_SECONDS`].
use std::collections::{HashMap, VecDeque};

use serde_json::{json, Value};

use super::journal::Journal;
use super::util::{id, seconds};
use super::Inner;

/// Replies the agent is shown, the newest ones.
pub(super) const TOLD_REPLIES: usize = 3;
/// Characters of each reply the agent is shown.
pub(super) const TOLD_CHARS: usize = 280;
/// Replies remembered per conversation; older ones are only counted.
const KEPT_PER_THREAD: usize = 64;
/// How long a person who came back may stay silent before the agent is sent a note.
pub(super) const NOTE_GRACE_SECONDS: u64 = 5;

/// The reasons a reply that was not played to the end ends unheard with.
const UNHEARD_REASONS: [&str; 8] = [
    "user_interrupted",
    "newer_turn",
    "unheard",
    "focus_changed",
    "call_ended",
    "session_changed",
    "queue_full",
    "playback_failed",
];

/// Whether a reply row with this status and reason was not heard; `Some(cut)` if so.
pub(super) fn unheard(status: &str, reason: Option<&str>) -> Option<bool> {
    match status {
        "text_only" => Some(false),
        "interrupted" | "failed" => reason
            .filter(|reason| UNHEARD_REASONS.contains(reason))
            .map(|reason| reason == "user_interrupted"),
        _ => None,
    }
}

#[derive(Default)]
struct Pending {
    /// Row ids, oldest first, each with whether it was cut while playing.
    rows: VecDeque<(String, bool)>,
    /// Unheard replies no longer in `rows`.
    overflow: usize,
}

/// A note waiting to tell an agent what its returning person did not hear.
pub(super) struct Note {
    pub(super) id: String,
    pub(super) session: String,
    pub(super) due: u64,
    pub(super) attempts: usize,
    /// What it tells, taken from the conversation's list when it is first sent.
    pub(super) told: Option<Value>,
}

#[derive(Default)]
pub(super) struct Unheard {
    by_thread: HashMap<String, Pending>,
    notes: HashMap<String, Note>,
}

impl Unheard {
    /// A reply on `thread` ended unheard (`Some(cut)`) or was heard (`None`).
    pub(super) fn update(&mut self, thread: &str, row_id: &str, unheard: Option<bool>) {
        match unheard {
            Some(cut) => {
                let pending = self.by_thread.entry(thread.to_owned()).or_default();
                if let Some(entry) = pending.rows.iter_mut().find(|(id, _)| id == row_id) {
                    entry.1 |= cut;
                    return;
                }
                pending.rows.push_back((row_id.to_owned(), cut));
                if pending.rows.len() > KEPT_PER_THREAD {
                    pending.rows.pop_front();
                    pending.overflow += 1;
                }
            }
            None => {
                if let Some(pending) = self.by_thread.get_mut(thread) {
                    pending.rows.retain(|(id, _)| id != row_id);
                }
            }
        }
    }

    pub(super) fn has(&self, thread: &str) -> bool {
        self.by_thread
            .get(thread)
            .is_some_and(|pending| pending.overflow > 0 || !pending.rows.is_empty())
    }

    /// What `thread`'s agent is told, emptying the list: how many replies went unheard, and the
    /// newest [`TOLD_REPLIES`] of them in the order they were published, each cut to [`TOLD_CHARS`].
    pub(super) fn take(&mut self, thread: &str, journal: &Journal) -> Option<Value> {
        let pending = self.by_thread.remove(thread)?;
        let count = pending.overflow + pending.rows.len();
        if count == 0 {
            return None;
        }
        let mut rows: Vec<_> = pending
            .rows
            .iter()
            .filter_map(|(row_id, cut)| Some((journal.find(row_id)?, *cut)))
            .collect();
        rows.sort_by_key(|(row, _)| row.seq);
        let shown = rows.len().saturating_sub(TOLD_REPLIES);
        let replies: Vec<Value> = rows[shown..]
            .iter()
            .map(|(row, cut)| {
                let truncated = row.text.chars().count() > TOLD_CHARS;
                let text: String = row.text.chars().take(TOLD_CHARS).collect();
                json!({"text":text,"truncated":truncated,"cut":cut})
            })
            .collect();
        Some(json!({"count":count,"replies":replies}))
    }

    /// A person came back to `thread` in call `session`: if they missed something, the agent is
    /// told unless they speak first.
    pub(super) fn offer_note(&mut self, thread: &str, session: &str) {
        if !self.has(thread) || self.notes.contains_key(thread) {
            return;
        }
        self.notes.insert(
            thread.to_owned(),
            Note {
                id: format!("note:{}", id()),
                session: session.to_owned(),
                due: seconds() + NOTE_GRACE_SECONDS,
                attempts: 0,
                told: None,
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

    /// The conversation a note was sent for, by its id.
    pub(super) fn note_thread(&self, note_id: &str) -> Option<String> {
        self.notes
            .iter()
            .find(|(_, note)| note.id == note_id)
            .map(|(thread, _)| thread.clone())
    }
}

impl Inner {
    /// Keep a reply's place on its conversation's list after its row changed.
    pub(super) fn track_unheard(&mut self, row_id: &str) {
        let Some(row) = self
            .journal
            .find(row_id)
            .filter(|row| row.role == "assistant")
        else {
            return;
        };
        let verdict = unheard(&row.status, row.reason.as_deref());
        if verdict.is_some() || row.status == "playback_finished" {
            let thread = row.thread.clone();
            self.unheard.update(&thread, row_id, verdict);
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
