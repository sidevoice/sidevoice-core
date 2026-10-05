//! What the link reports about itself to the room and the connector.

use serde_json::{json, Value};

use super::pairing::Pairing;

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct LinkState {
    room: Option<String>,
    connected: bool,
    via: Option<&'static str>,
    error: Option<String>,
    refused: Option<String>,
    public_url: Option<String>,
    linked: Option<Pairing>,
}

impl LinkState {
    pub(super) fn view(&self) -> Value {
        json!({"room":self.room,"connected":self.connected,"via":self.via,
            "error":self.error,"refused":self.refused})
    }

    pub(super) fn is_refused(&self) -> bool {
        self.refused.is_some()
    }

    /// The room's public URL, only while the link is up for this same pairing.
    pub(super) fn public_url_for(&self, pairing: &Pairing) -> Option<&str> {
        (self.linked.as_ref() == Some(pairing))
            .then(|| self.public_url.as_deref())
            .flatten()
    }

    pub(super) fn repaired(&mut self, room: Option<String>) {
        self.room = room;
        self.connected = false;
        self.via = None;
        self.error = None;
        self.refused = None;
        self.public_url = None;
        self.linked = None;
    }

    pub(super) fn connect(
        &mut self,
        via: &'static str,
        public_url: Option<String>,
        linked: Option<Pairing>,
    ) {
        self.connected = true;
        self.via = Some(via);
        self.error = None;
        self.refused = None;
        self.public_url = public_url;
        self.linked = linked;
    }

    /// Only the route that is up may take the link down. Returns whether it did.
    pub(super) fn disconnect(&mut self, via: &'static str) -> bool {
        if self.via != Some(via) {
            return false;
        }
        self.connected = false;
        self.via = None;
        self.public_url = None;
        self.linked = None;
        true
    }

    pub(super) fn refuse(&mut self, reason: &str) {
        self.connected = false;
        self.refused = Some(reason.chars().take(200).collect());
        self.error = None;
        self.via = None;
        self.public_url = None;
        self.linked = None;
    }

    pub(super) fn fail(&mut self, error: String) {
        self.error = Some(error);
        self.connected = false;
        self.via = None;
    }
}
