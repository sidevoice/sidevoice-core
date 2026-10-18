//! One owner for connector credentials, bindings and process-local conversation state.
//!
//! `Room` holds that state behind one lock. Each area of the state is a type of its own that
//! keeps its collections private; the room's operations are split across the submodules below
//! by concern and reach the state only through those types.
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Mutex;

use super::telemetry;
use crate::storage::PrivateDir;

// The areas of the room's state.
mod bindings;
mod browsers;
mod client_errors;
mod conversations;
mod credentials;
mod inflight;
mod journal;
mod latency_log;
mod peers;
mod unheard;
mod utterances;

// The room's operations, by concern.
mod channel;
mod connections;
mod declaration;
mod error;
mod focus;
mod hearing;
mod input;
mod latency;
mod notices;
mod pairing;
mod participants;
mod playback;
mod pull;
mod push;
mod registration;
mod replay;
mod reports;
mod sessions;
mod snapshot;
mod speech;
mod turn_flow;
mod turns;
mod util;

pub use error::RoomError;
pub use latency::{latency_now_micros, LatencyDuration, LatencyEvent, LatencyMark, LatencyReply};
pub use peers::{ConnectorPeer, PeerError, PeerRequest};
pub use turns::{Continuation, VoiceTurn};

use bindings::Bindings;
use browsers::Browsers;
use client_errors::ClientErrors;
use conversations::Conversations;
use credentials::Credentials;
use inflight::Inflight;
use journal::Journal;
use latency_log::LatencyLog;
use notices::PendingNotice;
use peers::Peers;
use unheard::Unheard;
use utterances::Utterances;

struct Inner {
    credentials: Credentials,
    /// A voluntary Sidevoice leave survives Core restart; a missing connector never adds a thread here.
    closed_threads: HashSet<String>,
    peers: Peers,
    bindings: Bindings,
    browsers: Browsers,
    conversations: Conversations,
    journal: Journal,
    utterances: Utterances,
    unheard: Unheard,
    inflight: Inflight,
    latency: LatencyLog,
    client_errors: ClientErrors,
    refusals: HashMap<String, (Value, Value)>,
    refusal_order: std::collections::VecDeque<String>,
    pending_notices: std::collections::VecDeque<PendingNotice>,
}

pub struct Room {
    dir: PrivateDir,
    inner: Mutex<Inner>,
}
impl Room {
    pub fn load(dir: PrivateDir) -> io::Result<Self> {
        let state = dir
            .read_json("room-state.json")?
            .unwrap_or_else(|| serde_json::json!({"connectors": {}}));
        let credentials = Credentials::from_state(&state);
        let closed_threads = state
            .get("closed_threads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|thread| util::valid_thread(thread))
            .map(str::to_owned)
            .collect();
        Ok(Self {
            dir,
            inner: Mutex::new(Inner {
                credentials,
                closed_threads,
                peers: Peers::default(),
                bindings: Bindings::default(),
                browsers: Browsers::default(),
                conversations: Conversations::default(),
                journal: Journal::default(),
                utterances: Utterances::default(),
                unheard: Unheard::default(),
                inflight: Inflight::default(),
                latency: LatencyLog::new(telemetry::shared()),
                client_errors: ClientErrors::default(),
                refusals: HashMap::new(),
                refusal_order: std::collections::VecDeque::new(),
                pending_notices: std::collections::VecDeque::new(),
            }),
        })
    }
    fn save(&self, credentials: &Credentials, closed_threads: &HashSet<String>) -> io::Result<()> {
        let mut state = credentials.state();
        let mut closed: Vec<_> = closed_threads.iter().cloned().collect();
        closed.sort();
        state["closed_threads"] = serde_json::json!(closed);
        self.dir.write_json("room-state.json", &state)
    }
}

#[cfg(test)]
mod tests;
