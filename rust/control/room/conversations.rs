//! Conversation-local speech boundaries. A call revision remains a routing key; only a
//! qualified spoken start changes the epoch shared by every device on a conversation.
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Default)]
pub(super) struct Conversations {
    epochs: HashMap<String, u64>,
    open: HashMap<String, HashSet<(String, String)>>,
    completed: HashMap<(String, String), u64>,
    completed_order: VecDeque<(String, String)>,
    empty_ends: VecDeque<(String, u64, String)>,
}

/// Process-local cap: retain every issued epoch for these conversations so a thread
/// cannot silently restart at zero while this room is alive.
const MAX_EPOCH_CONTEXTS: usize = 2048;

impl Conversations {
    pub(super) fn epoch(&self, thread: &str) -> u64 {
        self.epochs.get(thread).copied().unwrap_or(0)
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

    pub(super) fn record_empty(&mut self, thread: &str, epoch: u64, session: &str) {
        self.empty_ends
            .push_back((thread.to_owned(), epoch, session.to_owned()));
        while self.empty_ends.len() > 4096 {
            self.empty_ends.pop_front();
        }
    }

    pub(super) fn empty_after(&self, thread: &str, epoch: u64) -> Option<(u64, String)> {
        self.empty_ends
            .iter()
            .filter(|(candidate, newer, _)| candidate == thread && *newer > epoch)
            .min_by_key(|(_, newer, _)| *newer)
            .map(|(_, newer, session)| (*newer, session.clone()))
    }
}
