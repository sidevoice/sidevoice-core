//! One owner for connector credentials, bindings and process-local conversation state.
//!
//! `Room` is the single owner of that state, held behind one lock; its operations are split
//! across the submodules below by concern.
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use super::telemetry::Telemetry;
use crate::storage::PrivateDir;

mod bindings;
mod capabilities;
mod credentials;
mod error;
mod focus;
mod input;
mod journal;
mod latency;
mod peers;
mod playback;
mod pull;
mod push;
mod receipts;
mod replay;
mod reports;
mod sessions;
mod snapshot;
mod speech;
mod turns;
mod util;

pub use error::RoomError;
pub use latency::{latency_now_micros, LatencyDuration, LatencyEvent, LatencyMark, LatencyReply};
pub use peers::{ConnectorPeer, PeerError, PeerRequest};
pub use turns::VoiceTurn;

use bindings::Binding;
use journal::Row;
use sessions::Browser;
use speech::UtteranceRecord;

struct Inner {
    telemetry: Option<Arc<Telemetry>>,
    connectors: Map<String, Value>,
    pairing: HashMap<String, u64>,
    peers: HashMap<String, ConnectorPeer>,
    peer_order: VecDeque<String>,
    bindings: HashMap<String, Binding>,
    browsers: HashMap<String, Browser>,
    sessions: VecDeque<String>,
    rows: VecDeque<Row>,
    utterances: HashMap<String, UtteranceRecord>,
    client_errors: VecDeque<Value>,
    seq: u64,
    working: HashMap<String, bool>,
    inflight: HashMap<String, String>,
    latency_marks: HashMap<String, VecDeque<LatencyMark>>,
    latency_replies: HashMap<String, VecDeque<LatencyReply>>,
    latency_input: HashMap<(String, String, u64), Vec<LatencyDuration>>,
}

pub struct Room {
    dir: PrivateDir,
    inner: Mutex<Inner>,
}
impl Room {
    pub fn load(dir: PrivateDir) -> io::Result<Self> {
        let state = match dir.read_json("room-state.json")? {
            Some(v) => v,
            None => dir.import_legacy_connectors()?,
        };
        let connectors = state
            .get("connectors")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            dir,
            inner: Mutex::new(Inner {
                telemetry: Telemetry::from_env().map(Arc::new),
                connectors,
                pairing: HashMap::new(),
                peers: HashMap::new(),
                peer_order: VecDeque::new(),
                bindings: HashMap::new(),
                browsers: HashMap::new(),
                sessions: VecDeque::new(),
                rows: VecDeque::new(),
                utterances: HashMap::new(),
                client_errors: VecDeque::new(),
                seq: 0,
                working: HashMap::new(),
                inflight: HashMap::new(),
                latency_marks: HashMap::new(),
                latency_replies: HashMap::new(),
                latency_input: HashMap::new(),
            }),
        })
    }
    fn save(&self, inner: &Inner) -> io::Result<()> {
        self.dir
            .write_json("room-state.json", &json!({"connectors": inner.connectors}))
    }
}

#[cfg(test)]
mod tests;
