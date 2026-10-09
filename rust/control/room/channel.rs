//! Closing a conversation's channel from the room: its binding ends, its unsent input is
//! withdrawn and every call focused on it is moved off it.
use serde_json::{json, Value};

use super::browsers::Target;
use super::error::RoomError;
use super::peers::ConnectorPeer;
use super::util::valid_thread;
use super::Room;

impl Room {
    pub fn close_channel(
        &self,
        thread: &str,
    ) -> Result<(Value, Option<(ConnectorPeer, Value)>), RoomError> {
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let binding = inner
            .bindings
            .newest_active(thread)
            .map(|b| (b.id.clone(), b.connector.clone()));
        if binding.is_none() && !inner.journal.has_thread(thread) {
            return Err(RoomError::new(409, "room.conversation_missing"));
        }
        let notify = binding.as_ref().and_then(|(bid, cid)| {
            inner.peers.get(cid).cloned().map(|peer| {
                (
                    peer,
                    json!({"binding_id":bid,"thread":thread,"reason":"closed_from_room"}),
                )
            })
        });
        if let Some((bid, _)) = binding.as_ref() {
            if let Some(b) = inner.bindings.get_mut(bid) {
                b.end();
            }
            inner.inflight.finish(bid);
        }
        for input in inner.journal.cancel_unsent(thread, "channel_closed") {
            inner.browsers.input_receipt(&input, "not_sent");
        }
        for sid in inner.browsers.ids_on_thread(thread) {
            if let Some(browser) = inner.browsers.get_mut(&sid) {
                browser.refocus(Target::none());
            }
            inner.interrupt_client(&sid, "focus_changed");
        }
        let binding_id = binding.map(|(bid, _)| bid);
        Ok((json!({"status":"closed","binding_id":binding_id}), notify))
    }
}
