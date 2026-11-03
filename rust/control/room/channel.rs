//! Closing a conversation's channel from the room: its binding ends, its unsent input is
//! withdrawn and every call focused on it is moved off it.
use serde_json::{json, Value};

use super::browsers::Target;
use super::error::RoomError;
use super::peers::ConnectorPeer;
use super::util::valid_thread;
use super::{Inner, Room};

impl Room {
    pub fn close_channel(
        &self,
        thread: &str,
    ) -> Result<(Value, Vec<(ConnectorPeer, Value)>), RoomError> {
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut guard = self.inner.lock().expect("room lock");
        self.close_locked(&mut guard, thread)
    }

    /// One device stops showing `thread`. The room closes the conversation, through the normal close, once no device
    /// seen in the last 30 days still shows it; until then it stays open for the others.
    pub fn remove_for_device(
        &self,
        thread: &str,
        device: &str,
    ) -> Result<(Value, Vec<(ConnectorPeer, Value)>), RoomError> {
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        inner.shown.hide(thread, device);
        if inner.shown.any_showing(thread) {
            return match self.save(inner) {
                Ok(()) => Ok((json!({"status":"removed","closed":false}), Vec::new())),
                Err(_) => Err(RoomError::new(500, "room.state_save_failed")),
            };
        }
        let (mut result, notify) = self.close_locked(inner, thread)?;
        result["closed"] = json!(true);
        Ok((result, notify))
    }

    fn close_locked(
        &self,
        inner: &mut Inner,
        thread: &str,
    ) -> Result<(Value, Vec<(ConnectorPeer, Value)>), RoomError> {
        let bindings: Vec<_> = inner
            .bindings
            .active()
            .filter(|b| b.thread == thread)
            .map(|b| (b.id.clone(), b.connector.clone()))
            .collect();
        // A person's remembered card can be closed while its machine has no loaded binding after a restart.
        // Persist that intent before acknowledging the close; old binding replay must not undo it.
        inner.shown.forget(thread);
        if inner.closed_threads.insert(thread.to_owned()) && self.save(inner).is_err() {
            inner.closed_threads.remove(thread);
            return Err(RoomError::new(500, "room.state_save_failed"));
        }
        let notify = bindings
            .iter()
            .filter_map(|(bid, cid)| {
                inner.peers.get(cid).cloned().map(|peer| {
                    (
                        peer,
                        json!({"binding_id":bid,"thread":thread,"reason":"closed_from_room"}),
                    )
                })
            })
            .collect();
        for (bid, _) in &bindings {
            if let Some(b) = inner.bindings.get_mut(bid) {
                b.end();
            }
            inner.inflight.finish(bid);
        }
        for input in inner.journal.cancel_unsent(thread, "channel_closed") {
            inner.browsers.input_receipt(&input, "not_sent");
            inner.salvage_input_notice(&input.id);
        }
        for sid in inner.browsers.ids_on_thread(thread) {
            if let Some(browser) = inner.browsers.get_mut(&sid) {
                browser.refocus(Target::none());
            }
            inner.withdraw(&sid, "focus_changed", |_, _| true);
        }
        let binding_id = bindings.first().map(|(bid, _)| bid.clone());
        Ok((json!({"status":"closed","binding_id":binding_id}), notify))
    }
}
