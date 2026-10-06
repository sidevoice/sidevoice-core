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
        let requested = field(data, "binding_id");
        let pull_input = match field(data, "input_mode") {
            "" | "push" => false,
            "pull" => true,
            _ => return Err(RoomError::new(400, "room.input_mode_invalid")),
        };
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
        binding.renew(Declaration::parse(data), pull_input);
        let actual_thread = binding.thread.clone();
        inner.release_pull_claims(|row| {
            row.thread == actual_thread
                && (!pull_input || row.pull_claimed_by.as_deref() != Some(bid.as_str()))
        });
        Ok(
            json!({"client_ref": data.get("client_ref"), "binding_id": bid, "thread": actual_thread}),
        )
    }
    pub fn unregister(&self, cid: &str, bid: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(binding) = inner.bindings.get_mut(bid).filter(|b| b.connector == cid) {
            binding.end();
            inner.release_pull_claims(|row| row.pull_claimed_by.as_deref() == Some(bid));
        }
    }
}
