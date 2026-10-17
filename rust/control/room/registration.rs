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
        let client_ref = field(data, "client_ref");
        if data["voice_contract"] == 1 && client_ref.is_empty() {
            return Err(RoomError::new(400, "room.client_ref_invalid"));
        }
        // A mode the room does not know registers as push, as one that names no mode does.
        let pull_input = field(data, "input_mode") == "pull";
        let bid = match inner.bindings.get(requested) {
            Some(existing)
                if existing.connector != cid
                    || (data["voice_contract"] == 1
                        && !existing.client_ref.is_empty()
                        && existing.client_ref != client_ref) =>
            {
                return Err(RoomError::new(409, "room.binding_foreign"));
            }
            Some(existing) if existing.room_closed && !requested.is_empty() => {
                if data["voice_contract"] == 1 {
                    return Ok(json!({"status":"closed","reason":"binding_closed",
                        "binding_id":requested,"voice_contract":1}));
                }
                return Err(RoomError::new(409, "room.binding_closed"));
            }
            Some(_) if !requested.is_empty() => requested.to_owned(),
            _ => inner
                .bindings
                .newest_active_of(cid, thread)
                .filter(|b| data["voice_contract"] != 1 || b.client_ref == client_ref)
                .map_or_else(id, |b| b.id.clone()),
        };
        let binding = inner.bindings.get_or_create(&bid, cid, thread);
        binding.renew(Declaration::parse(data), pull_input, client_ref);
        let actual_thread = binding.thread.clone();
        let current_epoch = inner.conversations.epoch(&actual_thread);
        let initial_session = if data["voice_contract"] == 1 {
            inner.voice_contract.initial_session(&bid, current_epoch)
        } else {
            format!("binding:{bid}")
        };
        inner.release_pull_claims(|row| {
            row.thread == actual_thread
                && (!pull_input || row.pull_claimed_by.as_deref() != Some(bid.as_str()))
        });
        let mut result = json!({"client_ref": data.get("client_ref"), "binding_id": bid, "thread": actual_thread,
                "epoch":inner.conversations.epoch(&actual_thread),
                "initial_reply_context":{"session_id":initial_session,"revision":0,
                    "epoch":inner.conversations.epoch(&actual_thread),"thread_id":actual_thread}});
        if data["voice_contract"] == 1 {
            result["voice_contract"] = json!(1);
            result["voice_contract_instance"] = json!(inner.voice_contract.instance());
        }
        Ok(result)
    }
    pub fn unregister(&self, cid: &str, bid: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(binding) = inner.bindings.get_mut(bid).filter(|b| b.connector == cid) {
            binding.end();
            inner.contract_binding_lost(bid);
            inner.release_pull_claims(|row| row.pull_claimed_by.as_deref() == Some(bid));
        }
    }
}
