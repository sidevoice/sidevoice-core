//! A browser's focus: which conversation it is talking to, and switching it.
use serde_json::{json, Value};

use super::error::RoomError;
use super::playback::interrupt_client;
use super::util::{id, valid_thread};
use super::Room;

#[derive(Clone)]
pub(super) struct Target {
    pub(super) thread: String,
    pub(super) title: Option<String>,
    pub(super) binding_id: String,
}
impl Target {
    pub(super) fn view(&self) -> Value {
        json!({"thread_id": self.thread, "title": self.title, "binding_id": self.binding_id})
    }
}

impl Room {
    pub fn select(&self, sid: &str, thread: &str) -> Result<Value, RoomError> {
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let Some(binding) = inner
            .bindings
            .values()
            .filter(|b| b.active && b.thread == thread)
            .max_by_key(|b| b.created)
        else {
            return Err(RoomError::new(409, "room.conversation_disconnected"));
        };
        let title = binding.title.clone();
        let Some(client) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if client.target.as_ref().is_some_and(|t| t.thread == thread) {
            return Ok(
                json!({"status":"already_active","binding":client.target.as_ref().map(Target::view)}),
            );
        }
        client.revision += 1;
        client.active = None;
        client.speaking = false;
        client.sent = 0;
        let _ = client.sender.try_send(
            json!({"type":"voice-cancel","data":{"session_id":sid,"revision":client.revision}}),
        );
        let target = Target {
            thread: thread.into(),
            title,
            binding_id: id(),
        };
        let view = target.view();
        client.target = Some(target);
        interrupt_client(&mut inner, sid, "focus_changed");
        Ok(json!({"status":"activated","binding":view}))
    }
    pub fn deselect(&self, sid: &str, bid: &str) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(c) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if c.target.as_ref().is_none_or(|t| t.binding_id != bid) {
            return Err(RoomError::new(409, "room.focus_changed"));
        }
        c.revision += 1;
        c.active = None;
        c.speaking = false;
        let _ = c.sender.try_send(
            json!({"type":"voice-cancel","data":{"session_id":sid,"revision":c.revision}}),
        );
        let target = Target {
            thread: String::new(),
            title: None,
            binding_id: id(),
        };
        let view = target.view();
        c.target = Some(target);
        interrupt_client(&mut inner, sid, "focus_changed");
        Ok(json!({"status":"activated","binding":view}))
    }
}
