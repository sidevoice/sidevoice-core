//! Connector peers: the live connection to each paired machine, and its attach/detach lifecycle.
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, watch};

use super::pull::release_pull_claims;
use super::Room;

#[derive(Debug)]
pub struct PeerError;
pub struct PeerRequest {
    pub method: String,
    pub params: Value,
    pub answer: Option<oneshot::Sender<Result<Value, PeerError>>>,
}
#[derive(Clone)]
pub struct ConnectorPeer {
    pub generation: String,
    pub sender: mpsc::Sender<PeerRequest>,
    pub stop: watch::Sender<bool>,
}
impl ConnectorPeer {
    pub fn disconnect(&self) {
        let _ = self.stop.send(true);
    }
    pub async fn send(&self, method: &str, params: Value) -> Result<(), PeerError> {
        self.sender
            .send(PeerRequest {
                method: method.into(),
                params,
                answer: None,
            })
            .await
            .map_err(|_| PeerError)
    }
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, PeerError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(PeerRequest {
                method: method.into(),
                params,
                answer: Some(tx),
            })
            .await
            .map_err(|_| PeerError)?;
        tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| PeerError)?
            .map_err(|_| PeerError)?
    }
}

impl Room {
    pub fn attach(&self, cid: &str, peer: ConnectorPeer) -> Option<ConnectorPeer> {
        let mut inner = self.inner.lock().expect("room lock");
        let old = inner.peers.insert(cid.into(), peer);
        inner.peer_order.retain(|id| id != cid);
        inner.peer_order.push_back(cid.into());
        let ids: Vec<String> = inner
            .bindings
            .values_mut()
            .filter(|b| b.connector == cid && b.active)
            .map(|b| {
                b.live = false;
                b.id.clone()
            })
            .collect();
        release_pull_claims(&mut inner, |row| {
            row.pull_claimed_by
                .as_ref()
                .is_some_and(|claim| ids.contains(claim))
        });
        for bid in ids {
            if let Some(row_id) = inner.inflight.remove(&bid) {
                if let Some(row) = inner.rows.iter_mut().find(|r| r.id == row_id) {
                    row.status = "pending".into();
                    row.next_attempt = 0;
                }
            }
        }
        old
    }
    pub fn detach(&self, cid: &str, generation: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        if inner
            .peers
            .get(cid)
            .is_none_or(|p| p.generation != generation)
        {
            return;
        }
        inner.peers.remove(cid);
        inner.peer_order.retain(|id| id != cid);
        let ids: Vec<String> = inner
            .bindings
            .values_mut()
            .filter(|b| b.connector == cid)
            .map(|b| {
                b.live = false;
                b.id.clone()
            })
            .collect();
        release_pull_claims(&mut inner, |row| {
            row.pull_claimed_by
                .as_ref()
                .is_some_and(|claim| ids.contains(claim))
        });
        for bid in ids {
            if let Some(row_id) = inner.inflight.remove(&bid) {
                if let Some(row) = inner.rows.iter_mut().find(|r| r.id == row_id) {
                    row.status = "pending".into();
                    row.next_attempt = 0;
                }
            }
        }
    }
    pub fn current_peer(&self, cid: &str, generation: &str) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .peers
            .get(cid)
            .is_some_and(|peer| peer.generation == generation)
    }
    pub fn connector_peer(&self) -> Option<ConnectorPeer> {
        let inner = self.inner.lock().expect("room lock");
        inner
            .peer_order
            .iter()
            .rev()
            .find_map(|cid| inner.peers.get(cid).cloned())
    }
    pub async fn rendezvous_changed(&self, state: Value) {
        let peers: Vec<ConnectorPeer> = self
            .inner
            .lock()
            .expect("room lock")
            .peers
            .values()
            .cloned()
            .collect();
        for peer in peers {
            let _ = peer.send("node.rendezvous", state.clone()).await;
        }
    }
    pub fn latest_connector_identity(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let Some(row) = inner
            .peer_order
            .iter()
            .rev()
            .find_map(|id| inner.connectors.get(id))
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
