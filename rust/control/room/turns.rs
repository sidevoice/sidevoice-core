//! Voice turns: opening, finishing and cancelling the user's spoken turn in a call.
use serde_json::{json, Value};

use super::error::RoomError;
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
        let revision = c.next_revision(sid);
        c.turn_revision = revision;
        c.speaking = true;
        c.cancelled_turn = None;
        let bound = c.bound_target();
        let result = VoiceTurn {
            session_id: sid.into(),
            revision,
            thread_id: bound.map(|t| t.thread.clone()),
            binding_id: bound.map(|t| t.binding_id.clone()),
            title: c.target.as_ref().and_then(|t| t.title.clone()),
            language: c.language.clone(),
        };
        inner.hold_client(sid, revision);
        Ok(result)
    }
    pub fn finish_turn(&self, sid: &str, revision: u64) {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if let Some(c) = inner.browsers.get_mut(sid) {
            if c.turn_revision == revision && c.speaking {
                c.speaking = false;
                c.quiet_until = Some(std::time::Instant::now() + c.audio_grace);
            }
        }
        for (uid, row_id) in inner.utterances.waiting_for_turn(sid, revision) {
            let Some(row) = inner.journal.find(&row_id) else {
                continue;
            };
            let Some(browser) = inner.browsers.get(sid) else {
                continue;
            };
            if browser.speaking || browser.revision != revision || !browser.is_on(&row.thread) {
                continue;
            }
            inner.utterances.set_client_status(&uid, sid, "queued");
            if inner.utterances.get(&uid).is_some_and(|r| !r.is_replay()) {
                inner.sync_row(&row_id, "queued", None);
            }
        }
        inner.dispatch_client(sid);
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
        browser.notify(json!({"type":"voice-user-turn","data":{
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
