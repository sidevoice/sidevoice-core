//! Utterances: how far each reply got in every call it was spoken to, and the replays of
//! earlier replies.
use std::collections::{HashMap, HashSet};

/// Most original utterances the room keeps; replays are bounded separately.
pub(super) const MAX_UTTERANCES: usize = 2048;

/// What one call did with an utterance: the revision it was sent under and how far it got there.
pub(super) type ClientEntry = (u64, String);

/// How an utterance ended on one call: why it stopped short, and how many of its characters were heard, when said.
#[derive(Clone, Debug, Default)]
pub(super) struct End {
    pub(super) reason: Option<String>,
    pub(super) heard_chars: Option<u64>,
}

#[derive(Default)]
pub(super) struct UtteranceRecord {
    pub(super) row_id: String,
    pub(super) clients: HashMap<String, ClientEntry>,
    /// How it ended on each call that ended it, by session; the journal row is derived from these and `clients`.
    pub(super) ends: HashMap<String, End>,
    pub(super) parked: bool,
    /// For a replay: the utterance it repeats, by id; one never sent to a call has no record of its own.
    pub(super) replay_of: Option<String>,
    /// Calls that heard this reply through: it played to the end, or that listener stopped it.
    pub(super) heard: HashSet<String>,
    /// Calls it was sent to. A call that was away when it was published never got it.
    pub(super) sent: HashSet<String>,
    /// A repetition the person asked for from the bubble, not a catch-up.
    pub(super) requested: bool,
}
impl UtteranceRecord {
    pub(super) fn is_replay(&self) -> bool {
        self.replay_of.is_some()
    }
    /// The furthest status any call reached; it is what the reply's journal row shows.
    pub(super) fn best_status(&self) -> Option<&str> {
        self.clients
            .values()
            .map(|(_, status)| status.as_str())
            .max_by_key(|status| status_rank(status))
    }
    /// What the reply's journal row shows: the furthest status any call reached, with the reason and the heard extent
    /// of the calls that ended there. A cut while it played (`user_interrupted`) is the reason told first, since its
    /// start was heard; the furthest any of those calls heard is how far it was heard.
    pub(super) fn outcome(&self) -> Option<(&str, Option<&str>, Option<u64>)> {
        let best = self.best_status()?;
        let ends: Vec<&End> = self
            .clients
            .iter()
            .filter(|(_, (_, status))| status == best)
            .filter_map(|(sid, _)| self.ends.get(sid))
            .collect();
        let reason = ends
            .iter()
            .filter_map(|end| end.reason.as_deref())
            .min_by_key(|reason| (*reason != "user_interrupted", *reason));
        let heard_chars = ends.iter().filter_map(|end| end.heard_chars).max();
        Some((best, reason, heard_chars))
    }
    /// Whether no call will still play this utterance.
    fn finished(&self) -> bool {
        self.clients.values().all(|(_, status)| {
            matches!(
                status.as_str(),
                "failed" | "playback_finished" | "interrupted"
            )
        })
    }
}

fn status_rank(status: &str) -> u8 {
    match status {
        "failed" => 2,
        "interrupted" => 3,
        "queued" => 4,
        "playing" => 7,
        "playback_finished" => 8,
        _ => 0,
    }
}

fn in_flight(status: &str) -> bool {
    matches!(status, "queued" | "playing")
}

#[derive(Default)]
pub(super) struct Utterances {
    by_id: HashMap<String, UtteranceRecord>,
}
impl Utterances {
    pub(super) fn get(&self, uid: &str) -> Option<&UtteranceRecord> {
        self.by_id.get(uid)
    }
    pub(super) fn get_mut(&mut self, uid: &str) -> Option<&mut UtteranceRecord> {
        self.by_id.get_mut(uid)
    }
    #[cfg(test)]
    pub(super) fn contains(&self, uid: &str) -> bool {
        self.by_id.contains_key(uid)
    }
    pub(super) fn insert(&mut self, uid: &str, record: UtteranceRecord) {
        self.by_id.insert(uid.to_owned(), record);
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = (&String, &UtteranceRecord)> {
        self.by_id.iter()
    }
    pub(super) fn original_count(&self) -> usize {
        self.by_id.values().filter(|r| !r.is_replay()).count()
    }
    pub(super) fn replay_count(&self) -> usize {
        self.by_id.values().filter(|r| r.is_replay()).count()
    }
    /// The original utterance spoken from a journal row, never one of its replays.
    pub(super) fn original_of_row(&self, row_id: &str) -> Option<(&String, &UtteranceRecord)> {
        self.by_id
            .iter()
            .find(|(_, record)| record.row_id == row_id && !record.is_replay())
    }
    /// One call's entry for an utterance moves to `status`, ending there as `end` says.
    pub(super) fn set_client_status(&mut self, uid: &str, sid: &str, status: &str, end: End) {
        if let Some(record) = self.by_id.get_mut(uid) {
            if let Some(entry) = record.clients.get_mut(sid) {
                entry.1 = status.into();
                record.ends.insert(sid.to_owned(), end);
            }
        }
    }
    /// Whether a call has an utterance in one of `statuses`.
    pub(super) fn has_client_status(&self, sid: &str, statuses: &[&str]) -> bool {
        self.by_id.values().any(|record| {
            record
                .clients
                .get(sid)
                .is_some_and(|(_, status)| statuses.contains(&status.as_str()))
        })
    }
    /// Stop everything a call still had to play, for `reason`. Returns the rows of the original utterances it
    /// stopped.
    pub(super) fn interrupt_client(&mut self, sid: &str, reason: &str) -> Vec<String> {
        let mut rows = Vec::new();
        for record in self.by_id.values_mut() {
            if let Some(entry) = record.clients.get_mut(sid) {
                if in_flight(&entry.1) {
                    entry.1 = "interrupted".into();
                    record.ends.insert(
                        sid.to_owned(),
                        End {
                            reason: Some(reason.to_owned()),
                            heard_chars: None,
                        },
                    );
                    if !record.is_replay() {
                        rows.push(record.row_id.clone());
                    }
                }
            }
        }
        rows
    }
    /// A catch-up that actually sounded for `sid` says so on the reply it repeated, so the next
    /// return does not offer it again. One that never started playing says nothing.
    pub(super) fn replay_heard(&mut self, original: &str, sid: &str, previous: &str, next: &str) {
        if next == "playback_finished" || (next == "interrupted" && previous == "playing") {
            if let Some(record) = self.by_id.get_mut(original) {
                record.heard.insert(sid.to_owned());
            }
        }
    }
    /// Forget the utterances spoken from journal rows that were dropped.
    pub(super) fn forget_rows(&mut self, row_ids: &[String]) {
        if !row_ids.is_empty() {
            self.by_id.retain(|_, u| !row_ids.contains(&u.row_id));
        }
    }
    pub(super) fn forget(&mut self, uid: &str) {
        self.by_id.remove(uid);
    }
    /// Forget the replays a call that left was given.
    pub(super) fn forget_replays_of(&mut self, sid: &str) {
        self.by_id
            .retain(|_, record| !record.is_replay() || !record.clients.contains_key(sid));
    }
    /// Forget replays that no call will play any more; originals stay for history and replay.
    pub(super) fn retire_finished_replays(&mut self) {
        self.by_id
            .retain(|_, record| !record.is_replay() || !record.finished());
    }
}
