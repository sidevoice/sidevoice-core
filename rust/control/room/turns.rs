//! Voice turns: opening, finishing and cancelling the user's spoken turn in a call.
use serde_json::{json, Value};

use super::error::RoomError;
use super::playback::{dispatch_client, hold_client, sync_row};
use super::Room;

/// Focus and revision captured atomically when a browser opens a voice turn.
#[derive(Clone, Debug)]
pub struct VoiceTurn {
    pub session_id: String,
    pub revision: u64,
    pub thread_id: Option<String>,
    pub binding_id: Option<String>,
    pub title: Option<String>,
    pub(super) language: String,
}

impl Room {
    pub fn begin_turn(&self, sid: &str) -> Result<VoiceTurn, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(c) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        c.revision += 1;
        c.turn_revision = c.revision;
        c.speaking = true;
        c.cancelled_turn = None;
        let _ = c.sender.try_send(
            json!({"type":"voice-cancel","data":{"session_id":sid,"revision":c.revision}}),
        );
        let result = VoiceTurn {
            session_id: sid.into(),
            revision: c.revision,
            thread_id: c
                .target
                .as_ref()
                .filter(|t| !t.thread.is_empty())
                .map(|t| t.thread.clone()),
            binding_id: c
                .target
                .as_ref()
                .filter(|t| !t.thread.is_empty())
                .map(|t| t.binding_id.clone()),
            title: c.target.as_ref().and_then(|t| t.title.clone()),
            language: c.language.clone(),
        };
        hold_client(&mut inner, sid, result.revision);
        Ok(result)
    }
    pub fn finish_turn(&self, sid: &str, revision: u64) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(c) = inner.browsers.get_mut(sid) {
            if c.turn_revision == revision {
                c.speaking = false;
            }
        }
        let waiting: Vec<(String, String)> = inner
            .utterances
            .iter()
            .filter(|(_, u)| {
                u.clients
                    .get(sid)
                    .is_some_and(|(rev, status)| *rev == revision && status == "waiting_for_turn")
            })
            .map(|(uid, u)| (uid.clone(), u.row_id.clone()))
            .collect();
        for (uid, row_id) in waiting {
            let Some(row) = inner.rows.iter().find(|r| r.id == row_id) else {
                continue;
            };
            let thread = row.thread.clone();
            let Some(browser) = inner.browsers.get(sid) else {
                continue;
            };
            if browser.speaking
                || browser.revision != revision
                || browser.target.as_ref().is_none_or(|t| t.thread != thread)
            {
                continue;
            }
            if let Some(record) = inner.utterances.get_mut(&uid) {
                if let Some(entry) = record.clients.get_mut(sid) {
                    entry.1 = "queued".into();
                }
            }
            if inner
                .utterances
                .get(&uid)
                .is_some_and(|record| record.replay_of.is_none())
            {
                sync_row(&mut inner, &row_id, "queued", None);
            }
        }
        dispatch_client(&mut inner, sid);
    }

    pub fn cancel_input(&self, sid: &str, revision: u64) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        if !browser.speaking || browser.turn_revision != revision {
            return Err(RoomError::new(409, "room.input_ended"));
        }
        browser.cancelled_turn = Some(revision);
        let thread = browser.target.as_ref().map(|target| target.thread.clone());
        let _ = browser
            .sender
            .try_send(json!({"type":"voice-user-turn","data":{
            "phase":"cancelled","revision":revision,"thread_id":thread}}));
        Ok(json!({"status":"cancelled"}))
    }

    pub fn turn_cancelled(&self, sid: &str, revision: u64) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .browsers
            .get(sid)
            .is_some_and(|browser| browser.cancelled_turn == Some(revision))
    }
}
