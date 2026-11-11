//! Conversation-local speech boundaries. A call revision remains a routing key; only a
//! qualified spoken start changes the epoch shared by every device on a conversation.
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use super::Inner;

#[derive(Default)]
pub(super) struct Conversations {
    epochs: HashMap<String, u64>,
    open: HashMap<String, HashSet<(String, String)>>,
    completed: HashMap<(String, String), u64>,
    completed_order: VecDeque<(String, String)>,
    /// Per conversation, the epochs of turns that came to nothing (no words, cancelled) since the last turn that
    /// brought words: the agent saw nothing of them.
    empty: HashMap<String, BTreeSet<u64>>,
}

/// Process-local cap: retain every issued epoch for these conversations so a thread
/// cannot silently restart at zero while this room is alive.
const MAX_EPOCH_CONTEXTS: usize = 2048;
/// How many epochs of turns that came to nothing a conversation keeps: older ones make a reply late again.
const MAX_EMPTY_EPOCHS: usize = 256;

impl Conversations {
    pub(super) fn epoch(&self, thread: &str) -> u64 {
        self.epochs.get(thread).copied().unwrap_or(0)
    }

    /// A restored message of epoch `epoch` makes the conversation's epoch at least that, so its reply is current.
    pub(super) fn restore_epoch(&mut self, thread: &str, epoch: u64) {
        let known = self.epochs.entry(thread.to_owned()).or_insert(0);
        *known = (*known).max(epoch);
    }

    pub(super) fn can_begin(&self, thread: &str) -> bool {
        self.epochs.contains_key(thread) || self.epochs.len() < MAX_EPOCH_CONTEXTS
    }

    pub(super) fn ensure(&mut self, thread: &str) -> bool {
        if !self.can_begin(thread) {
            return false;
        }
        self.epochs.entry(thread.to_owned()).or_insert(0);
        true
    }
    #[cfg(test)]
    pub(super) fn context_count(&self) -> usize {
        self.epochs.len()
    }

    pub(super) fn begin(&mut self, thread: &str, session: &str, turn: &str) -> u64 {
        let epoch = self.epochs.entry(thread.to_owned()).or_default();
        *epoch = epoch.saturating_add(1);
        self.open
            .entry(thread.to_owned())
            .or_default()
            .insert((session.to_owned(), turn.to_owned()));
        *epoch
    }

    pub(super) fn finish(&mut self, thread: &str, session: &str, turn: &str, epoch: u64) {
        if let Some(open) = self.open.get_mut(thread) {
            open.remove(&(session.to_owned(), turn.to_owned()));
            if open.is_empty() {
                self.open.remove(thread);
            }
        }
        let key = (session.to_owned(), turn.to_owned());
        if self.completed.insert(key.clone(), epoch).is_none() {
            self.completed_order.push_back(key);
        }
        while self.completed_order.len() > 4096 {
            if let Some(old) = self.completed_order.pop_front() {
                self.completed.remove(&old);
            }
        }
    }

    /// The turn of epoch `epoch` on `thread` came to nothing: no words, or cancelled.
    pub(super) fn came_to_nothing(&mut self, thread: &str, epoch: u64) {
        let empty = self.empty.entry(thread.to_owned()).or_default();
        empty.insert(epoch);
        while empty.len() > MAX_EMPTY_EPOCHS {
            empty.pop_first();
        }
    }

    /// The turn of epoch `epoch` on `thread` brought words: a reply of an earlier epoch is late from now on, so the
    /// turns before it that came to nothing no longer matter.
    pub(super) fn brought_words(&mut self, thread: &str, epoch: u64) {
        if let Some(empty) = self.empty.get_mut(thread) {
            empty.retain(|&nothing| nothing > epoch);
            if empty.is_empty() {
                self.empty.remove(thread);
            }
        }
    }

    /// Whether the turn of epoch `epoch` on `thread` came to nothing.
    pub(super) fn was_empty(&self, thread: &str, epoch: u64) -> bool {
        self.empty
            .get(thread)
            .is_some_and(|empty| empty.contains(&epoch))
    }

    pub(super) fn prior(&self, session: &str, turn: &str) -> Option<u64> {
        self.completed
            .get(&(session.to_owned(), turn.to_owned()))
            .copied()
    }

    pub(super) fn is_open(&self, thread: &str) -> bool {
        self.open.get(thread).is_some_and(|turns| !turns.is_empty())
    }
    pub(super) fn open_count(&self) -> usize {
        self.open.values().map(HashSet::len).sum()
    }
}

impl Inner {
    /// Whether the agent can have seen none of `thread`'s turns after `epoch`, up to `current`: each came to nothing
    /// (no words, cancelled) or ended as a message that was never sent. Its reply to the last message it got is then
    /// not for an older turn.
    pub(super) fn unseen_turns_since(&self, thread: &str, epoch: u64, current: u64) -> bool {
        (epoch + 1..=current).all(|turn| {
            self.conversations.was_empty(thread, turn) || self.journal.abandoned_turn(thread, turn)
        })
    }

    /// Register a conversation-local speech boundary; what it does to replies follows in `turn_flow`.
    pub(super) fn advance_speech_epoch(&mut self, thread: &str, sid: &str, turn_id: &str) -> u64 {
        assert!(
            self.conversations.can_begin(thread),
            "epoch context reserved before turn admission"
        );
        self.conversations.begin(thread, sid, turn_id)
    }
}
