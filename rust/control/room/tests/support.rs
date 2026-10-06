//! Fixtures shared by the room's tests.
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::control::room::util::id;
use crate::control::room::{ConnectorPeer, Room, RoomError};
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

pub(super) fn say(room: &Room, sid: &str, text: &str) {
    let turn = room.begin_turn(sid).unwrap();
    room.queue_voice_input(&turn, text).unwrap();
}

pub(super) fn pull(room: &Room, cid: &str, request: Value) -> Result<Value, RoomError> {
    room.pull_input(cid, &request)
}
