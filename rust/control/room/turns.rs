//! Voice turns: the person's spoken turn in a call, as the call's voice module reports it. A turn opens with the
//! focus it is spoken to and ends with the words it became, or with none.
use serde_json::{json, Value};

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
        c.cancelled_turn = None;
        let bound = c.bound_target();
        let turn = VoiceTurn {
            session_id: sid.into(),
            revision,
            thread_id: bound.map(|t| t.thread.clone()),
            binding_id: bound.map(|t| t.binding_id.clone()),
            title: c.target.as_ref().and_then(|t| t.title.clone()),
            language: c.language.clone(),
        };
        c.turn = Some(turn.clone());
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
            let Some(turn) = browser.turn.take_if(|turn| turn.revision == revision) else {
                return Err(RoomError::new(409, "room.input_ended"));
            };
            if browser.turn_revision == revision {
                browser.speaking = false;
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

    /// The person cancelled the turn they are speaking, from the page: its words, when they come, are dropped.
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
        browser.notify(json!({"type":"voice-user-turn","data":{"session_id":sid,
            "phase":"cancelled","revision":revision,"thread_id":thread}}));
        Ok(json!({"status":"cancelled"}))
    }
}
