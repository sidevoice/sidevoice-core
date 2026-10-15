//! Fixtures shared by the room's tests.
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::control::room::util::id;
use crate::control::room::{ConnectorPeer, Continuation, Room, RoomError, VoiceTurn};
use crate::storage::PrivateDir;

pub(super) fn pull_room(connectors: &[&str]) -> (tempfile::TempDir, Room, Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
    let mut generations = Vec::new();
    for cid in connectors {
        let (requests, _receiver) = mpsc::channel(4);
        let (stop, _stopped) = watch::channel(false);
        let generation = id();
        room.attach(
            cid,
            ConnectorPeer {
                generation: generation.clone(),
                sender: requests,
                stop,
            },
        );
        generations.push(generation);
    }
    (directory, room, generations)
}

/// Supply the focus identity captured at the start of each test turn.
pub(super) trait BeginAtFocus {
    fn begin_at_focus(&self, sid: &str, turn_id: &str) -> Result<VoiceTurn, RoomError>;
}

impl BeginAtFocus for Room {
    fn begin_at_focus(&self, sid: &str, turn_id: &str) -> Result<VoiceTurn, RoomError> {
        let snapshot = self.snapshot(Some(sid));
        let binding = &snapshot["binding"];
        self.begin_turn(
            sid,
            turn_id,
            binding["thread_id"].as_str().unwrap_or(""),
            binding["binding_id"].as_str().unwrap_or(""),
        )
    }
}

/// The person says `text` in call `sid`: a turn starts and ends with those words, as the call's voice module reports.
pub(super) fn say(room: &Room, sid: &str, text: &str) {
    let turn = room
        .begin_at_focus(sid, &crate::control::room::util::id())
        .unwrap();
    room.finish_turn(
        sid,
        &turn.turn_id,
        Some(text),
        &Value::Null,
        Continuation::Standalone,
    )
    .unwrap();
}

/// Call `sid` reports what became of reply `uid`.
pub(super) fn report(room: &Room, sid: &str, uid: &str, status: &str) {
    room.playback(sid, uid, status, None, None, &Value::Null)
        .unwrap();
}

pub(super) fn pull(room: &Room, cid: &str, request: Value) -> Result<Value, RoomError> {
    room.pull_input(cid, &request)
}
