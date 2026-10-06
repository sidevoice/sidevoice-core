//! The pinned connector's links on the Unix listener, Socket.IO v2 and JSON-RPC v3,
//! and what both protocols share: attaching as the room's peer and the room calls
//! they forward.

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::control::room::{ConnectorPeer, PeerRequest, Room};

use super::AppState;

mod v2;
mod v3;

pub(super) use v2::layer as v2_layer;

/// Requests the room may queue for a connector before it has read them.
const PEER_QUEUE: usize = 128;

pub(super) fn local_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/connectors/v3", get(v3::upgrade))
}

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// One connection's claim on a connector id; a newer connection supersedes it.
#[derive(Clone)]
struct Link {
    cid: String,
    generation: String,
}

impl Link {
    fn current(&self, room: &Room) -> bool {
        room.current_peer(&self.cid, &self.generation)
    }

    fn detach(&self, room: &Room) {
        room.detach(&self.cid, &self.generation);
    }
}

/// What a connection receives when it becomes the connector's peer.
struct Attached {
    link: Link,
    /// The room's requests and notifications for the connector.
    requests: mpsc::Receiver<PeerRequest>,
    /// Changes when the room stops this peer.
    stopped: watch::Receiver<bool>,
}

/// Attaches a connection as `cid`'s peer, disconnecting the one it replaces.
fn attach(room: &Room, cid: String) -> Attached {
    let generation = Uuid::new_v4().to_string();
    let (sender, requests) = mpsc::channel::<PeerRequest>(PEER_QUEUE);
    let (stop, stopped) = watch::channel(false);
    let peer = ConnectorPeer {
        generation: generation.clone(),
        sender,
        stop,
    };
    if let Some(old) = room.attach(&cid, peer) {
        old.disconnect();
    }
    Attached {
        link: Link { cid, generation },
        requests,
        stopped,
    }
}

/// Connector messages that change the room and expect no answer.
#[derive(Clone, Copy)]
enum Notification {
    Unregister,
    Working,
    Engine,
    Read,
}

impl Notification {
    const ALL: [Self; 4] = [Self::Unregister, Self::Working, Self::Engine, Self::Read];

    fn method(self) -> &'static str {
        match self {
            Self::Unregister => "binding.unregister",
            Self::Working => "input.working",
            Self::Engine => "input.engine",
            Self::Read => "input.read",
        }
    }

    fn parse(method: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|notification| notification.method() == method)
    }

    fn apply(self, room: &Room, cid: &str, params: &Value) {
        match self {
            Self::Unregister => room.unregister(cid, field(params, "binding_id")),
            Self::Working => room.working(cid, params),
            Self::Engine => room.engine(cid, params),
            Self::Read => room.read(cid, params),
        }
    }
}

fn register_binding(room: &Room, cid: &str, params: &Value) -> Value {
    room.register(cid, params)
        .unwrap_or_else(|e| json!({"error":e.key}))
}

/// Copies the connector's own identifiers into its answer so it can correlate it.
fn echo_ids(mut answer: Value, params: &Value, keys: &[&str]) -> Value {
    if let Some(obj) = answer.as_object_mut() {
        for key in keys {
            obj.insert(
                (*key).into(),
                params.get(*key).cloned().unwrap_or(Value::Null),
            );
        }
    }
    answer
}
