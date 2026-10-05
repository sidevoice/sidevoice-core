//! Utterances: how far each reply got in every call it was spoken to, and the replays of
//! earlier replies.
use std::collections::HashMap;

/// Most original utterances the room keeps; replays are bounded separately.
pub(super) const MAX_UTTERANCES: usize = 2048;

/// What one call is doing with an utterance: the revision it was queued under and its status.
pub(super) type ClientEntry = (u64, String);

pub(super) struct UtteranceRecord {
    pub(super) row_id: String,
    pub(super) clients: HashMap<String, ClientEntry>,
    pub(super) parked: bool,
    pub(super) replay_of: Option<String>,
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
        "waiting_for_turn" => 5,
        "playing" => 7,
        "playback_finished" => 8,
        _ => 0,
    }
}

fn in_flight(status: &str) -> bool {
    matches!(status, "queued" | "waiting_for_turn" | "playing")
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
    /// The furthest status of the utterance spoken from a journal row.
    pub(super) fn row_status(&self, row_id: &str) -> Option<&str> {
        self.by_id
            .values()
            .find(|record| record.row_id == row_id)
            .and_then(UtteranceRecord::best_status)
    }
    /// A call's entry for an utterance: the revision it was queued under and its status.
    pub(super) fn client_entry(&self, uid: &str, sid: &str) -> Option<&ClientEntry> {
        self.by_id
            .get(uid)
            .and_then(|record| record.clients.get(sid))
    }
    pub(super) fn set_client_status(&mut self, uid: &str, sid: &str, status: &str) {
        if let Some(entry) = self
            .by_id
            .get_mut(uid)
            .and_then(|record| record.clients.get_mut(sid))
        {
            entry.1 = status.into();
        }
    }
    /// The utterances a call holds waiting for the end of its turn `revision`, with their rows.
    pub(super) fn waiting_for_turn(&self, sid: &str, revision: u64) -> Vec<(String, String)> {
        self.by_id
            .iter()
            .filter(|(_, u)| {
                u.clients
                    .get(sid)
                    .is_some_and(|(rev, status)| *rev == revision && status == "waiting_for_turn")
            })
            .map(|(uid, u)| (uid.clone(), u.row_id.clone()))
            .collect()
    }
    /// Stop everything a call still had to play. Returns the rows of original utterances that
    /// no call is playing any more.
    pub(super) fn interrupt_client(&mut self, sid: &str) -> Vec<String> {
        let mut rows = Vec::new();
        for record in self.by_id.values_mut() {
            if let Some(entry) = record.clients.get_mut(sid) {
                if in_flight(&entry.1) {
                    entry.1 = "interrupted".into();
                    if !record.is_replay()
                        && record
                            .clients
                            .values()
                            .all(|(_, status)| !in_flight(status))
                    {
                        rows.push(record.row_id.clone());
                    }
                }
            }
        }
        rows
    }
    /// The user started turn `revision` in a call: what it had queued waits for the turn to end
    /// and what it was playing is interrupted.
    pub(super) fn hold_client(&mut self, sid: &str, revision: u64) -> Held {
        let mut held = Held::default();
        for (uid, record) in &mut self.by_id {
            let Some(entry) = record.clients.get_mut(sid) else {
                continue;
            };
            match entry.1.as_str() {
                "queued" | "waiting_for_turn" => {
                    entry.0 = revision;
                    entry.1 = "waiting_for_turn".into();
                    held.waiting.push((
                        uid.clone(),
                        (!record.is_replay()).then(|| record.row_id.clone()),
                    ));
                }
                "playing" => {
                    entry.1 = "interrupted".into();
                    if !record.is_replay() {
                        held.interrupted.push(record.row_id.clone());
                    }
                }
                _ => {}
            }
        }
        held
    }
    /// Forget the utterances spoken from journal rows that were dropped.
    pub(super) fn forget_rows(&mut self, row_ids: &[String]) {
        if !row_ids.is_empty() {
            self.by_id.retain(|_, u| !row_ids.contains(&u.row_id));
        }
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

/// What holding a call for a new turn changed.
#[derive(Default)]
pub(super) struct Held {
    /// Utterances now waiting for the turn to end, with the row of each original.
    pub(super) waiting: Vec<(String, Option<String>)>,
    /// Rows of original utterances whose playback was cut.
    pub(super) interrupted: Vec<String>,
}
