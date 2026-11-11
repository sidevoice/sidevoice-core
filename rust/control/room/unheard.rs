//! What the person did not hear of an agent's replies, told to the agent as one short block.
//!
//! A reply counts as heard only when an app confirmed it played to its end (`reply-hearing` `heard`, on any device of
//! the conversation: `globally_heard`). Anything else (cut, never played, written while nobody listened, left queued
//! when the call ended, an app killed mid-reply) is not heard. From the first reply of a conversation not heard in full,
//! every later one counts as not heard too: the agent is told how many replies that is, how the first one began, and,
//! when it was cut, up to where it was heard. Never the texts: the agent has them.
//!
//! The block goes with the person's next spoken message, or, when the person comes back to the conversation and says
//! nothing for [`NOTE_GRACE_SECONDS`], as one `user_returned` note. Once told, those replies are not told again
//! (`told_through`). Nothing is told per reply, and a late receipt never sends anything on its own.
use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::journal::{Journal, Row};
use super::util::seconds;
use super::Inner;

/// How long a person who came back may stay silent before the agent is sent a note.
pub(super) const NOTE_GRACE_SECONDS: u64 = 5;
/// The bytes the room keeps for one block in a message or note: it names no texts, only a few words of two.
pub(super) const BLOCK_BYTES: usize = 2048;
/// The most characters of a reply's opening the block quotes.
const OPENING_CHARS: usize = 60;
/// The most characters of the heard part of a cut reply the block quotes (its end).
const HEARD_TAIL_CHARS: usize = 120;

/// A note waiting to tell an agent what its returning person did not hear.
pub(super) struct Note {
    pub(super) due: u64,
    /// The journal position when the person came back: replies published after it are being heard now.
    pub(super) until: u64,
}

#[derive(Default)]
pub(super) struct Unheard {
    /// Per conversation, the journal position of the last reply the agent has been told about (or that was heard
    /// before a block was due): later blocks start after it.
    told_through: HashMap<String, u64>,
    notes: HashMap<String, Note>,
    empty_obligations: HashSet<(String, u64)>,
}

impl Unheard {
    /// The block for `thread`'s replies after the last one told, up to journal position `until`: none when every one of
    /// them was heard in full. Also returns the position of the last reply it covers.
    pub(super) fn block(
        &self,
        thread: &str,
        journal: &Journal,
        until: u64,
    ) -> Option<(Value, u64)> {
        let floor = self.told_through.get(thread).copied().unwrap_or(0);
        let replies: Vec<&Row> = journal
            .replies(thread)
            .filter(|row| row.seq > floor && row.seq <= until)
            .collect();
        let first = replies.iter().position(|row| !row.globally_heard)?;
        let missed = &replies[first..];
        let opening = missed[0];
        let mut block = json!({"count":missed.len(),"from":opening_words(&opening.text),
            "history_id":opening.id});
        if let Some(heard) = opening
            .heard_chars
            .filter(|heard| *heard > 0)
            .and_then(|heard| usize::try_from(heard).ok())
        {
            let heard: String = opening.text.chars().take(heard).collect();
            block["heard_up_to"] = json!(heard_tail(&heard));
        }
        Some((block, missed.last().map_or(floor, |row| row.seq)))
    }

    /// The agent was told about `thread`'s replies up to journal position `seq`.
    pub(super) fn told(&mut self, thread: &str, seq: u64) {
        let told = self.told_through.entry(thread.to_owned()).or_default();
        *told = (*told).max(seq);
    }

    /// A person came back to `thread`: if they missed something published until now, the agent is told unless they
    /// speak first.
    pub(super) fn offer_note(&mut self, thread: &str, until: u64) {
        let note = self.notes.entry(thread.to_owned()).or_insert(Note {
            due: seconds() + NOTE_GRACE_SECONDS,
            until,
        });
        // Back again before the note was due: what was published meanwhile was missed too.
        note.until = note.until.max(until);
    }

    /// The notes due at `now`, by conversation.
    pub(super) fn due_notes(&self, now: u64) -> Vec<(String, u64)> {
        self.notes
            .iter()
            .filter(|(_, note)| note.due <= now)
            .map(|(thread, note)| (thread.clone(), note.until))
            .collect()
    }

    pub(super) fn note_mut(&mut self, thread: &str) -> Option<&mut Note> {
        self.notes.get_mut(thread)
    }

    pub(super) fn drop_note(&mut self, thread: &str) {
        self.notes.remove(thread);
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

    /// The person spoke on `thread`: what they say carries the block, so no note waits any more.
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

/// How a reply began, for the agent to recognise it: its first words, cut at a word, never the whole reply.
pub(super) fn opening_words(full: &str) -> String {
    let chars = full.trim().chars().collect::<Vec<_>>();
    if chars.len() <= 1 {
        return "…".to_owned();
    }
    let limit = OPENING_CHARS.min(chars.len() - 1);
    let end = chars[..limit]
        .iter()
        .rposition(|ch| ch.is_whitespace())
        .filter(|index| *index > 0)
        .unwrap_or(limit);
    format!("{}…", chars[..end].iter().collect::<String>())
}

/// The end of what was heard of a cut reply, cut at a word when it is long.
pub(super) fn heard_tail(heard: &str) -> String {
    let chars = heard.chars().collect::<Vec<_>>();
    if chars.len() <= HEARD_TAIL_CHARS {
        return heard.to_owned();
    }
    let skip = chars.len() - HEARD_TAIL_CHARS;
    let start = chars
        .iter()
        .enumerate()
        .skip(skip)
        .take(40)
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(skip, |(index, _)| index + 1);
    format!("…{}", chars[start..].iter().collect::<String>())
}

impl Inner {
    /// A turn ended with no words. Only a reply refused while the person spoke (`empty_obligation`) makes it worth a
    /// note: the agent is told it can speak now. A turn that only cut a reply tells the agent nothing: the reply plays
    /// on, and what of it is still not heard is in the next block.
    pub(super) fn settle_empty_turn(&mut self, thread: &str, session: &str, epoch: u64) {
        if self.unheard.finish_empty(thread, epoch) {
            self.queue_unheard_note(thread, session, "turn_empty", None, None);
        }
    }

    /// The block for `thread` now, taken: the agent is told it with what it goes in.
    pub(super) fn take_unheard(&mut self, thread: &str, until: Option<u64>) -> Option<Value> {
        let until = until.unwrap_or_else(|| self.journal.last_seq());
        match self.unheard.block(thread, &self.journal, until) {
            Some((block, last)) => {
                self.unheard.told(thread, last);
                Some(block)
            }
            None => {
                // Everything so far was heard: a later block starts after it.
                let last = self
                    .journal
                    .replies(thread)
                    .map(|row| row.seq)
                    .filter(|seq| *seq <= until)
                    .last();
                if let Some(last) = last {
                    self.unheard.told(thread, last);
                }
                None
            }
        }
    }

    /// Call `sid` is back on its conversation: its agent is told what was missed there, unless the person speaks first.
    pub(super) fn offer_note(&mut self, sid: &str) {
        if let Some(thread) = self
            .browsers
            .get(sid)
            .and_then(|browser| browser.bound_target())
            .map(|target| target.thread.clone())
        {
            let until = self.journal.last_seq();
            self.unheard.offer_note(&thread, until);
        }
    }
}

#[cfg(test)]
impl Inner {
    /// Whether the agent would be told a block for `thread` now.
    pub(super) fn has_unheard(&self, thread: &str) -> bool {
        self.unheard
            .block(thread, &self.journal, self.journal.last_seq())
            .is_some()
    }
}
