//! A browser's focus: which conversation it is talking to, and switching it.
use serde_json::{json, Value};

use super::browsers::Target;
use super::error::RoomError;
use super::util::{id, valid_thread};
use super::{Inner, Room};

impl Room {
    pub fn select(&self, sid: &str, thread: &str) -> Result<Value, RoomError> {
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        // Selecting needs no active binding: the conversation may come
        // back, and what was missed on it is replayed by the caller.
        let title = inner
            .bindings
            .newest_active(thread)
            .and_then(|binding| binding.title.clone());
        let Some(client) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if client.is_on(thread) {
            return Ok(
                json!({"status":"already_active","binding":client.target.as_ref().map(Target::view)}),
            );
        }
        client.active = None;
        client.sent = 0;
        let target = Target {
            thread: thread.into(),
            title,
            binding_id: id(),
        };
        let view = target.view();
        client.refocus(sid, target);
        inner.interrupt_client(sid, "focus_changed");
        inner.report_working(sid);
        Ok(json!({"status":"activated","binding":view}))
    }
    /// Focus a call that just joined on the conversation its hello names, honoured only while
    /// that conversation is still in the room.
    pub fn restore_focus(&self, sid: &str, thread: &str) -> bool {
        if !valid_thread(thread) {
            return false;
        }
        let mut inner = self.inner.lock().expect("room lock");
        let Some(title) = inner
            .bindings
            .newest_active(thread)
            .map(|binding| binding.title.clone())
        else {
            return false;
        };
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return false;
        };
        browser.target = Some(Target {
            thread: thread.into(),
            title,
            binding_id: id(),
        });
        inner.report_working(sid);
        true
    }
    pub fn deselect(&self, sid: &str, bid: &str) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(c) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if c.target.as_ref().is_none_or(|t| t.binding_id != bid) {
            return Err(RoomError::new(409, "room.focus_changed"));
        }
        c.active = None;
        let target = Target::none();
        let view = target.view();
        c.refocus(sid, target);
        inner.interrupt_client(sid, "focus_changed");
        Ok(json!({"status":"activated","binding":view}))
    }
}

impl Inner {
    /// A call landing on a conversation mid-turn is told what its harness last said.
    fn report_working(&self, sid: &str) {
        let Some(browser) = self.browsers.get(sid) else {
            return;
        };
        let Some(thread) = browser.bound_target().map(|target| target.thread.as_str()) else {
            return;
        };
        if let Some(working) = self.bindings.working(thread) {
            browser.notify(
                json!({"type":"voice-conversation","data":{"thread_id":thread,"working":working}}),
            );
        }
    }
}
