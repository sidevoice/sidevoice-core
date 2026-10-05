//! Connector peers: the live connection to each paired machine, and the table of those that
//! are attached now.
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

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

/// The attached peers, one per connector, remembered in the order they attached.
#[derive(Default)]
pub(super) struct Peers {
    attached: HashMap<String, ConnectorPeer>,
    order: VecDeque<String>,
}
impl Peers {
    /// Attach `peer` as connector `cid`'s connection, returning the one it replaces.
    pub(super) fn attach(&mut self, cid: &str, peer: ConnectorPeer) -> Option<ConnectorPeer> {
        let old = self.attached.insert(cid.into(), peer);
        self.order.retain(|id| id != cid);
        self.order.push_back(cid.into());
        old
    }
    /// Detach connector `cid` if `generation` is still its connection; false if it was not.
    pub(super) fn detach(&mut self, cid: &str, generation: &str) -> bool {
        if !self.is_current(cid, generation) {
            return false;
        }
        self.attached.remove(cid);
        self.order.retain(|id| id != cid);
        true
    }
    pub(super) fn is_current(&self, cid: &str, generation: &str) -> bool {
        self.attached
            .get(cid)
            .is_some_and(|peer| peer.generation == generation)
    }
    pub(super) fn get(&self, cid: &str) -> Option<&ConnectorPeer> {
        self.attached.get(cid)
    }
    pub(super) fn contains(&self, cid: &str) -> bool {
        self.attached.contains_key(cid)
    }
    /// Connector IDs, most recently attached first.
    pub(super) fn recent(&self) -> impl Iterator<Item = &String> {
        self.order.iter().rev()
    }
    pub(super) fn latest(&self) -> Option<&ConnectorPeer> {
        self.recent().find_map(|cid| self.attached.get(cid))
    }
    pub(super) fn all(&self) -> Vec<ConnectorPeer> {
        self.attached.values().cloned().collect()
    }
}
