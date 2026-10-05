//! Browser sessions: joining and leaving the room, admission and per-call settings.
use std::collections::VecDeque;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::error::RoomError;
use super::focus::Target;
use super::playback::interrupt_client;
use super::util::{field, id, millis};
use super::Room;

const MAX_BROWSERS: usize = 8;

pub(super) struct Browser {
    pub(super) device: String,
    pub(super) language: String,
    pub(super) sender: mpsc::Sender<Value>,
    pub(super) target: Option<Target>,
    pub(super) revision: u64,
    pub(super) turn_revision: u64,
    pub(super) speaking: bool,
    pub(super) cancelled_turn: Option<u64>,
    pub(super) sent: u64,
    pub(super) active: Option<String>,
    pub(super) pending: VecDeque<String>,
}

impl Room {
    pub fn join(
        &self,
        device: String,
        language: String,
        sender: mpsc::Sender<Value>,
    ) -> Result<String, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let max = std::env::var("VOICE_MAX_BROWSERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(MAX_BROWSERS)
            .max(1);
        if inner.browsers.len() >= max {
            return Err(RoomError::new(429, "room.full"));
        }
        let sid = id();
        inner.sessions.push_back(sid.clone());
        if inner.sessions.len() > 64 {
            inner.sessions.pop_front();
        }
        inner.browsers.insert(
            sid.clone(),
            Browser {
                device,
                language,
                sender,
                target: None,
                revision: 0,
                turn_revision: 0,
                speaking: false,
                cancelled_turn: None,
                sent: 0,
                active: None,
                pending: VecDeque::new(),
            },
        );
        inner.latency_marks.insert(sid.clone(), VecDeque::new());
        inner.latency_replies.insert(sid.clone(), VecDeque::new());
        Ok(sid)
    }
    pub fn leave(&self, sid: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        inner.browsers.remove(sid);
        interrupt_client(&mut inner, sid, "call_ended");
        inner
            .utterances
            .retain(|_, record| record.replay_of.is_none() || !record.clients.contains_key(sid));
        inner.latency_marks.remove(sid);
        inner.latency_replies.remove(sid);
        inner
            .latency_input
            .retain(|(session, _, _), _| session != sid);
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
    pub fn report_client_error(&self, report: &Value) -> Value {
        let mut inner = self.inner.lock().expect("room lock");
        let clipped =
            |key: &str, max: usize| field(report, key).chars().take(max).collect::<String>();
        inner.client_errors.push_back(json!({"session_id":clipped("session_id",100),"kind":clipped("kind",40),"message":clipped("message",200),"at":millis()}));
        while inner.client_errors.len() > 20 {
            inner.client_errors.pop_front();
        }
        json!({"status":"recorded"})
    }
    pub fn admission(&self, language: &str) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let max = std::env::var("VOICE_MAX_BROWSERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(MAX_BROWSERS)
            .max(1);
        json!({"admitted":inner.browsers.len()<max,"reason":if inner.browsers.len()>=max {Some("room_is_full")} else {None},
        "message":if inner.browsers.len()>=max {Some(crate::messages::render(&crate::messages::LocalizedMessage::new("room.full"),language))} else {None},"clients":inner.browsers.len(),"max":max})
    }
}
