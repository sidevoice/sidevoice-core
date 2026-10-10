//! Replays: an earlier reply the person asked to hear again, sent to their call once more as text without
//! touching its history row.
use std::collections::HashMap;

use serde_json::{json, Value};

use super::error::RoomError;
use super::utterances::UtteranceRecord;
use super::Room;

pub(super) const MAX_REPLAY_RECORDS: usize = 16;

impl Room {
    /// Sends call `sid` the reply of row `history_id` again, as utterance `uid`; its voice module speaks it.
    pub fn replay_one(&self, sid: &str, history_id: &str, uid: &str) -> Result<Value, RoomError> {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if inner.utterances.replay_count() >= MAX_REPLAY_RECORDS {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let Some(row) = inner
            .journal
            .find(history_id)
            .filter(|row| row.role == "assistant")
        else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let thread = row.thread.clone();
        // The utterance it repeats; a reply saved without being sent to any call (superseded, or refused for a full
        // queue) has none, and is repeated by the id it was published with.
        let original = match inner.utterances.original_of_row(history_id) {
            Some((original, _)) => original.clone(),
            None => history_id
                .rsplit_once(":voice:")
                .map_or(history_id, |(_, uid)| uid)
                .to_owned(),
        };
        let Some(browser) = inner.browsers.get(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if !browser.is_on(&thread) {
            return Err(RoomError::new(404, "room.replay_missing"));
        }
        let revision = browser.revision;
        inner.utterances.insert(
            uid,
            UtteranceRecord {
                row_id: history_id.to_owned(),
                clients: HashMap::from([(sid.to_owned(), (revision, "queued".to_owned()))]),
                replay_of: Some(original),
                requested: true,
                ..Default::default()
            },
        );
        inner.send_reply(sid, uid);
        if !inner
            .utterances
            .get(uid)
            .is_some_and(|record| record.sent.contains(sid))
        {
            inner.utterances.forget(uid);
            return Err(RoomError::new(429, "room.replay_full"));
        }
        Ok(json!({"utterance_id":uid,"history_id":history_id}))
    }
}
