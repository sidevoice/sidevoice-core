//! One owner for connector credentials, bindings and process-local conversation state.
//!
//! `Room` holds that state behind one lock. Each area of the state is a type of its own that
//! keeps its collections private; the room's operations are split across the submodules below
//! by concern and reach the state only through those types.
use std::io;
use std::sync::{Arc, Mutex};

use super::telemetry::Telemetry;
use crate::storage::PrivateDir;

// The areas of the room's state.
mod bindings;
mod browsers;
mod client_errors;
mod credentials;
mod inflight;
mod journal;
mod latency_log;
mod peers;
mod utterances;

// The room's operations, by concern.
mod channel;
mod connections;
mod declaration;
mod error;
mod focus;
mod input;
mod latency;
mod pairing;
mod participants;
mod playback;
mod pull;
mod push;
mod receipts;
mod registration;
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
pub use replay::MissedReply;
pub use turns::VoiceTurn;

use bindings::Bindings;
use browsers::Browsers;
use client_errors::ClientErrors;
use credentials::Credentials;
use inflight::Inflight;
use journal::Journal;
use latency_log::LatencyLog;
use peers::Peers;
use utterances::Utterances;

struct Inner {
    credentials: Credentials,
    peers: Peers,
    bindings: Bindings,
    browsers: Browsers,
    journal: Journal,
    utterances: Utterances,
    inflight: Inflight,
    latency: LatencyLog,
    client_errors: ClientErrors,
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
        let credentials = Credentials::from_state(&state);
        Ok(Self {
            dir,
            inner: Mutex::new(Inner {
                credentials,
                peers: Peers::default(),
                bindings: Bindings::default(),
                browsers: Browsers::default(),
                journal: Journal::default(),
                utterances: Utterances::default(),
                inflight: Inflight::default(),
                latency: LatencyLog::new(Telemetry::from_env().map(Arc::new)),
                client_errors: ClientErrors::default(),
            }),
        })
    }
    fn save(&self, credentials: &Credentials) -> io::Result<()> {
        self.dir.write_json("room-state.json", &credentials.state())
    }
}

#[cfg(test)]
mod tests;
