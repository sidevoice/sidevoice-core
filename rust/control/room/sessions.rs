//! Browser sessions: joining and leaving the room, admission, parking and the call's language.
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::browsers::Browser;
use super::error::RoomError;
use super::util::id;
use super::Room;

const MAX_BROWSERS: usize = 8;

/// How many calls the room admits at once.
fn max_browsers() -> usize {
    std::env::var("VOICE_MAX_BROWSERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(MAX_BROWSERS)
        .max(1)
}

impl Room {
    /// Device output eligibility. Microphone mute is independent of this state.
    pub fn set_speech(
        &self,
        sid: &str,
        on: bool,
        reason: Option<&str>,
    ) -> Result<Value, RoomError> {
        if !on && !matches!(reason, Some("voice_off" | "agents_muted")) {
            return Err(RoomError::new(400, "room.speech_eligibility_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let changed = browser.speech_on != on || browser.speech_reason.as_deref() != reason;
        browser.speech_on = on;
        browser.speech_reason = if on { None } else { reason.map(str::to_owned) };
        if changed && !on {
            inner.withdraw(sid, reason.unwrap_or("voice_off"), |_, _| true);
        } else if changed && on {
            inner.offer_note(sid);
        }
        Ok(json!({"on":on,"reason":if on {None} else {reason}}))
    }
    pub fn join(
        &self,
        device: String,
        language: String,
        sender: mpsc::Sender<Value>,
    ) -> Result<String, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        if inner.browsers.len() >= max_browsers() {
            return Err(RoomError::new(429, "room.full"));
        }
        let sid = id();
        inner
            .browsers
            .join(&sid, Browser::new(device, language, sender));
        inner.latency.open(&sid);
        Ok(sid)
    }
    pub fn leave(&self, sid: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        let turns: Vec<_> = inner
            .browsers
            .get(sid)
            .map(|browser| browser.turns.iter().cloned().collect())
            .unwrap_or_default();
        for turn in turns {
            if let Some(thread) = turn.thread_id.as_deref() {
                inner
                    .conversations
                    .finish(thread, sid, &turn.turn_id, turn.epoch);
                inner.settle_empty_turn(thread, sid, turn.epoch);
            }
        }
        inner.browsers.leave(sid);
        inner.interrupt_client(sid, "call_ended");
        if let Some(telemetry) = super::telemetry::shared() {
            telemetry.call_ended(sid, "disconnected");
        }
        inner.utterances.forget_replays_of(sid);
        inner.latency.close(sid);
    }
    /// A call whose socket went keeps its seat, and nothing new is sent to it until its page is back.
    pub fn park(&self, sid: &str, parked: bool) {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return;
        };
        match (browser.parked, parked) {
            (None, true) => browser.parked = Some(std::time::Instant::now()),
            (Some(_), false) => browser.parked = None,
            _ => {}
        }
        if parked {
            inner.drop_unheard(sid, &[]);
        }
    }
    /// The page is back: what was published while it was away, and a reply handed to it that never
    /// arrived, are marked unheard instead of played, and its agent is told. Under one lock with the call's return:
    /// a reply published meanwhile is either marked unheard here or sent to the page that is back.
    pub fn resume(&self, sid: &str, unreceived: &[String]) {
        let mut inner = self.inner.lock().expect("room lock");
        inner.drop_unheard(sid, unreceived);
        inner.offer_note(sid);
        if let Some(browser) = inner.browsers.get_mut(sid) {
            browser.parked = None;
        }
    }
    pub fn owns_session(&self, sid: &str, device: &str) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .browsers
            .get(sid)
            .is_some_and(|browser| browser.device == device)
    }
    pub fn set_language(&self, sid: &str, language: &str) {
        if let Some(browser) = self.inner.lock().expect("room lock").browsers.get_mut(sid) {
            browser.language = language.to_owned();
        }
    }
    pub fn admission(&self, language: &str) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let max = max_browsers();
        let clients = inner.browsers.len();
        json!({"admitted":clients<max,"reason":if clients>=max {Some("room_is_full")} else {None},
        "message":if clients>=max {Some(crate::messages::render(&crate::messages::LocalizedMessage::new("room.full"),language))} else {None},"clients":clients,"max":max})
    }
}
