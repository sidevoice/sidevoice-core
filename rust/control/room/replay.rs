//! Replays: speaking an earlier reply again to one call without touching its history row.
use std::collections::HashMap;

use serde_json::{json, Value};

use super::error::RoomError;
use super::playback::{dispatch_client, MAX_PENDING};
use super::speech::UtteranceRecord;
use super::{Inner, Room};

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
        let Some(record) = inner
            .utterances
            .values()
            .find(|record| record.row_id == history_id && record.replay_of.is_none())
        else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let Some(row) = inner.rows.iter().find(|row| row.id == record.row_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        if browser.target.as_ref().map(|target| target.thread.as_str()) != Some(row.thread.as_str())
        {
            return Err(RoomError::new(404, "room.replay_missing"));
        }
        Ok((row.text.clone(), row.language.clone()))
    }

    pub fn replay_one(&self, sid: &str, history_id: &str, uid: &str) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        if inner
            .utterances
            .values()
            .filter(|record| record.replay_of.is_some())
            .count()
            >= MAX_REPLAY_RECORDS
        {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let Some(original) = inner
            .utterances
            .iter()
            .find(|(_, record)| record.row_id == history_id && record.replay_of.is_none())
            .map(|(uid, _)| uid.clone())
        else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let Some(row) = inner.rows.iter().find(|row| row.id == history_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let thread = row.thread.clone();
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if browser.target.as_ref().map(|target| target.thread.as_str()) != Some(thread.as_str()) {
            return Err(RoomError::new(404, "room.replay_missing"));
        }
        // Leave one queue slot for the next live reply, even during a replay burst.
        if browser.pending.len() >= MAX_PENDING - 1 {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let event = json!({"type":"voice-replay","data":{"session_id":sid,"thread_id":thread,
            "replies":[{"utterance_id":uid,"history_id":history_id}],"skipped":[]}});
        if browser.sender.try_send(event).is_err() {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let revision = browser.revision;
        browser.pending.push_front(uid.to_owned());
        inner.utterances.insert(
            uid.to_owned(),
            UtteranceRecord {
                row_id: history_id.to_owned(),
                clients: HashMap::from([(sid.to_owned(), (revision, "queued".to_owned()))]),
                parked: false,
                replay_of: Some(original),
            },
        );
        dispatch_client(&mut inner, sid);
        Ok(json!({"utterance_id":uid,"history_id":history_id}))
    }
    pub fn has_replay(&self, uid: &str) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .utterances
            .get(uid)
            .is_some_and(|record| record.replay_of.is_some())
    }
}

pub(super) fn retire_terminal_replays(inner: &mut Inner) {
    inner.utterances.retain(|_, record| {
        record.replay_of.is_none()
            || record.clients.values().any(|(_, status)| {
                !matches!(
                    status.as_str(),
                    "failed" | "playback_finished" | "interrupted"
                )
            })
    });
}
