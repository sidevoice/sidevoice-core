//! Voice turns: the person's spoken turn in a call, as the call's voice module reports it. A turn opens with the
//! focus it is spoken to and ends with the words it became, or with none. Several may wait for their words at once:
//! the person may start the next turn, or move to another conversation, while the last one is still transcribed.
use serde_json::{json, Value};

use super::browsers::MAX_OPEN_TURNS;
use super::error::RoomError;
use super::latency::{latency_now_micros, LatencyEvent};
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
    /// The person cancelled it: its words, when they come, are dropped.
    pub(super) cancelled: bool,
}

impl Room {
    /// The person started speaking in call `sid`: a new revision, and the focus the turn's words will go to.
    pub fn begin_turn(&self, sid: &str) -> Result<VoiceTurn, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(c) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let revision = c.next_revision();
        c.turn_revision = revision;
        c.speaking = true;
        c.open_turn = Some(revision);
        let bound = c.bound_target();
        let turn = VoiceTurn {
            session_id: sid.into(),
            revision,
            thread_id: bound.map(|t| t.thread.clone()),
            binding_id: bound.map(|t| t.binding_id.clone()),
            title: c.target.as_ref().and_then(|t| t.title.clone()),
            language: c.language.clone(),
            cancelled: false,
        };
        c.turns.push_back(turn.clone());
        if c.turns.len() > MAX_OPEN_TURNS {
            c.turns.pop_front();
        }
        Ok(turn)
    }

    /// Turn `revision` of call `sid` ended. With `text`, its words go to the conversation it was spoken to, as a
    /// message; without (cancelled, or nothing said), it just ends. `timings` are the call's own measures of it.
    pub fn finish_turn(
        &self,
        sid: &str,
        revision: u64,
        text: Option<&str>,
        timings: &Value,
    ) -> Result<Value, RoomError> {
        let turn = {
            let mut inner = self.inner.lock().expect("room lock");
            let Some(browser) = inner.browsers.get_mut(sid) else {
                return Err(RoomError::new(409, "room.browser_absent"));
            };
            let Some(index) = browser
                .turns
                .iter()
                .position(|turn| turn.revision == revision)
            else {
                return Err(RoomError::new(409, "room.input_ended"));
            };
            let turn = browser.turns.remove(index).expect("the turn found");
            if browser.open_turn == Some(revision) {
                browser.speaking = false;
                browser.open_turn = None;
            }
            turn
        };
        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
            return Ok(json!({"accepted":false,"revision":revision}));
        };
        if let Some(thread) = turn.thread_id.as_deref() {
            self.latency_mark(
                sid,
                thread,
                revision,
                None,
                LatencyEvent::Transcript,
                latency_now_micros(),
            );
            if let Some(timings) = timings.as_object() {
                for (name, milliseconds) in timings {
                    if let Some(milliseconds) = milliseconds.as_f64() {
                        self.latency_duration(sid, thread, revision, None, name, milliseconds);
                    }
                }
            }
        }
        self.queue_voice_input(&turn, text)
    }

    /// The person cancelled turn `revision`, from the page, while speaking it or before its words came: its words, when they come, are dropped.
    pub fn cancel_input(&self, sid: &str, revision: u64) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        let Some(turn) = browser
            .turns
            .iter_mut()
            .find(|turn| turn.revision == revision && !turn.cancelled)
        else {
            return Err(RoomError::new(409, "room.input_ended"));
        };
        turn.cancelled = true;
        let thread = turn.thread_id.clone();
        browser.notify(json!({"type":"voice-user-turn","data":{"session_id":sid,
            "phase":"cancelled","revision":revision,"thread_id":thread}}));
        Ok(json!({"status":"cancelled"}))
    }
}
