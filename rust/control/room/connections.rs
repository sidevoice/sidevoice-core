//! Connector connections: attaching and detaching a machine's peer, and what that does to the
//! bindings and deliveries that depended on it.
use serde_json::{json, Map, Value};

use super::peers::ConnectorPeer;
use super::{Inner, Room};

impl Room {
    pub fn attach(&self, cid: &str, peer: ConnectorPeer) -> Option<ConnectorPeer> {
        let mut inner = self.inner.lock().expect("room lock");
        let old = inner.peers.attach(cid, peer);
        let ids = inner.bindings.go_offline_active(cid);
        inner.bindings_went_offline(&ids);
        old
    }
    pub fn detach(&self, cid: &str, generation: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.peers.detach(cid, generation) {
            return;
        }
        inner.bindings.forget_working_of(cid);
        let ids = inner.bindings.go_offline(cid);
        inner.bindings_went_offline(&ids);
    }
    /// Whether any connector is linked: the core does not idle out from under one.
    pub fn has_connector(&self) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .peers
            .latest()
            .is_some()
    }
    pub fn current_peer(&self, cid: &str, generation: &str) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .peers
            .is_current(cid, generation)
    }
    pub fn connector_peer(&self) -> Option<ConnectorPeer> {
        self.inner
            .lock()
            .expect("room lock")
            .peers
            .latest()
            .cloned()
    }
    pub fn latest_connector_identity(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let Some(row) = inner
            .peers
            .recent()
            .find_map(|cid| inner.credentials.get(cid))
        else {
            return json!({});
        };
        let mut result = Map::new();
        for key in ["host", "platform", "version", "harnesses"] {
            if let Some(value) = row.get(key) {
                result.insert(key.to_owned(), value.clone());
            }
        }
        Value::Object(result)
    }
}

impl Inner {
    /// Claimed input stays delivered when a binding goes offline; a replacement binding may
    /// fetch it, but never inject it. A push delivery awaiting acknowledgement is retried.
    fn bindings_went_offline(&mut self, ids: &[String]) {
        for bid in ids {
            if let Some(row_id) = self.inflight.finish(bid) {
                if let Some(row) = self.journal.find_mut(&row_id) {
                    row.requeue();
                }
            }
        }
    }
}
