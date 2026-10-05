//! Replays: speaking an earlier reply again to one call without touching its history row.
use std::collections::HashMap;

use serde_json::{json, Value};

use super::error::RoomError;
use super::playback::MAX_PENDING;
use super::utterances::UtteranceRecord;
use super::Room;

pub(super) const MAX_REPLAY_RECORDS: usize = 16;

impl Room {
    pub fn replay_source(
        &self,
        sid: &str,
        history_id: &str,
    ) -> Result<(String, Option<String>), RoomError> {
        let inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let Some((_, record)) = inner.utterances.original_of_row(history_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let Some(row) = inner.journal.find(&record.row_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        if !browser.is_on(&row.thread) {
            return Err(RoomError::new(404, "room.replay_missing"));
        }
        Ok((row.text.clone(), row.language.clone()))
    }

    pub fn replay_one(&self, sid: &str, history_id: &str, uid: &str) -> Result<Value, RoomError> {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if inner.utterances.replay_count() >= MAX_REPLAY_RECORDS {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let Some((original, _)) = inner.utterances.original_of_row(history_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let original = original.clone();
        let Some(row) = inner.journal.find(history_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let thread = row.thread.clone();
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if !browser.is_on(&thread) {
            return Err(RoomError::new(404, "room.replay_missing"));
        }
        // Leave one queue slot for the next live reply, even during a replay burst.
        if browser.pending.len() >= MAX_PENDING - 1 {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let event = json!({"type":"voice-replay","data":{"session_id":sid,"thread_id":thread,
            "replies":[{"utterance_id":uid,"history_id":history_id}],"skipped":[]}});
        if !browser.offer(event) {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let revision = browser.revision;
        browser.pending.push_front(uid.to_owned());
        inner.utterances.insert(
            uid,
            UtteranceRecord {
                row_id: history_id.to_owned(),
                clients: HashMap::from([(sid.to_owned(), (revision, "queued".to_owned()))]),
                parked: false,
                replay_of: Some(original),
            },
        );
        inner.dispatch_client(sid);
        Ok(json!({"utterance_id":uid,"history_id":history_id}))
    }
    pub fn has_replay(&self, uid: &str) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .utterances
            .get(uid)
            .is_some_and(|record| record.is_replay())
    }
}
