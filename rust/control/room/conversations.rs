//! Conversation-local speech boundaries. A call revision remains a routing key; only a
//! qualified spoken start changes the epoch shared by every device on a conversation.
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Default)]
pub(super) struct Conversations {
    epochs: HashMap<String, u64>,
    open: HashMap<String, HashSet<(String, String)>>,
    completed: HashMap<(String, String), u64>,
    completed_order: VecDeque<(String, String)>,
}

impl Conversations {
    pub(super) fn epoch(&self, thread: &str) -> u64 {
        self.epochs.get(thread).copied().unwrap_or(0)
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
}
