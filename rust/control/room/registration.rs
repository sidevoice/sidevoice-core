//! Registration: a connector binding one of its agent threads to the room, and ending it.
use serde_json::{json, Value};

use super::bindings::Binding;
use super::declaration::Declaration;
use super::error::RoomError;
use super::util::{field, id, valid_thread};
use super::Room;

impl Room {
    pub fn binding_views(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        json!(inner
            .bindings
            .active()
            .map(Binding::view)
            .collect::<Vec<_>>())
    }
    pub fn register(&self, cid: &str, data: &Value) -> Result<Value, RoomError> {
        let thread = field(data, "thread");
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.peers.contains(cid) {
            return Err(RoomError::new(409, "room.connector_disconnected"));
        }
        if !inner.conversations.ensure(thread) {
            return Err(RoomError::new(429, "room.input_backlog_full"));
        }
        let requested = field(data, "binding_id");
        if let Some(existing) = inner
            .bindings
            .get(requested)
            .filter(|_| !requested.is_empty())
        {
            if existing.connector != cid {
                return Err(RoomError::new(409, "room.binding_foreign"));
            }
            if existing.thread != thread {
                return Err(RoomError::new(409, "room.binding_thread_mismatch"));
            }
        }
        if !requested.is_empty() && inner.closed_threads.contains(thread) {
            return Err(RoomError::new(409, "room.binding_closed"));
        }
        if requested.is_empty()
            && inner.closed_threads.remove(thread)
            && self
                .save(&inner.credentials, &inner.closed_threads)
                .is_err()
        {
            inner.closed_threads.insert(thread.to_owned());
            return Err(RoomError::new(500, "room.state_save_failed"));
        }
        let bid = match inner.bindings.get(requested) {
            Some(existing) if existing.connector != cid => {
                return Err(RoomError::new(409, "room.binding_foreign"));
            }
            Some(_) if !requested.is_empty() => requested.to_owned(),
            _ => inner
                .bindings
                .newest_active_of(cid, thread)
                .map_or_else(id, |b| b.id.clone()),
        };
        let binding = inner.bindings.get_or_create(&bid, cid, thread);
        binding.renew(Declaration::parse(data));
        let actual_thread = binding.thread.clone();
        Ok(
            json!({"client_ref": data.get("client_ref"), "binding_id": bid, "thread": actual_thread,
                "epoch":inner.conversations.epoch(&actual_thread),
                "initial_reply_context":{"session_id":format!("binding:{bid}"),"revision":0,
                    "epoch":inner.conversations.epoch(&actual_thread),"thread_id":actual_thread}}),
        )
    }
    pub fn unregister(&self, cid: &str, bid: &str) {
        let _ = self.unregister_with_thread(cid, bid, "", false);
    }
    /// `leave` names a deliberate agent-side exit. A reset or lost transport may unregister a binding without
    /// retiring the conversation; an offline leave can name its thread after Core has restarted.
    pub fn unregister_with_thread(
        &self,
        cid: &str,
        bid: &str,
        declared_thread: &str,
        leave: bool,
    ) -> Result<(), RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let binding = inner.bindings.get(bid);
        if binding.is_some_and(|binding| binding.connector != cid) {
            return Err(RoomError::new(409, "room.binding_foreign"));
        }
        let owned = binding.is_some();
        let thread = binding
            .map(|binding| binding.thread.clone())
            .or_else(|| valid_thread(declared_thread).then(|| declared_thread.to_owned()));
        if leave && thread.is_none() {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut save_failed = false;
        if leave {
            if let Some(thread) = thread.as_deref() {
                let other = inner
                    .bindings
                    .active()
                    .any(|b| b.thread == thread && (!owned || b.id != bid));
                if !other && inner.closed_threads.insert(thread.to_owned()) {
                    // This is a one-way connector notification. Keep the room closed in memory even if its
                    // private write fails; the Connector's durable leave is replayed on its next Core link.
                    save_failed = self
                        .save(&inner.credentials, &inner.closed_threads)
                        .is_err();
                }
            }
        }
        if let Some(binding) = inner.bindings.get_mut(bid).filter(|b| b.connector == cid) {
            binding.end();
        }
        if save_failed {
            Err(RoomError::new(500, "room.state_save_failed"))
        } else {
            Ok(())
        }
    }
}
