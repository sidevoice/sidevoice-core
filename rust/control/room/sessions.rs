//! Browser sessions: joining and leaving the room, admission and per-call settings.
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
        inner.browsers.leave(sid);
        inner.interrupt_client(sid, "call_ended");
        if let Some(telemetry) = super::telemetry::shared() {
            telemetry.call_ended(sid, "disconnected");
        }
        inner.utterances.forget_replays_of(sid);
        inner.latency.close(sid);
    }
    /// A call whose socket went keeps its seat; the reply it was given waits for it rather than
    /// running out its playback bound while nobody can confirm it, and nothing new is handed to it.
    pub fn park(&self, sid: &str, parked: bool) {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return;
        };
        match (browser.parked, parked) {
            (None, true) => browser.parked = Some(std::time::Instant::now()),
            (Some(since), false) => {
                browser.parked = None;
                if let Some((_, deadline)) = browser.playback_watch.as_mut() {
                    *deadline += since.elapsed();
                }
                inner.dispatch_client(sid);
            }
            _ => {}
        }
    }
    /// The page is back: what was published while it was away, and a reply handed to it that never
    /// arrived, are marked unheard instead of played.
    pub fn resume(&self, sid: &str, unreceived: &[String]) {
        let mut guard = self.inner.lock().expect("room lock");
        guard.drop_unheard(sid, unreceived);
        drop(guard);
        self.park(sid, false);
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
    pub fn set_audio_grace(&self, sid: &str, seconds: f32) {
        if let Some(browser) = self.inner.lock().expect("room lock").browsers.get_mut(sid) {
            browser.audio_grace = std::time::Duration::from_secs_f32(if seconds.is_finite() {
                seconds.clamp(0.0, 10.0)
            } else {
                1.0
            });
        }
    }
    pub fn set_transcription(&self, sid: &str, transcription: Value) {
        if let Some(browser) = self.inner.lock().expect("room lock").browsers.get_mut(sid) {
            browser.transcription = transcription;
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
