use tokio::sync::{mpsc, watch};

use crate::control::room::{ConnectorPeer, Room};
use crate::storage::PrivateDir;

#[test]
fn host_agent_peer_is_latest_live_connection() {
    let directory = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
    for (cid, generation) in [
        ("first", "first-1"),
        ("second", "second-1"),
        ("first", "first-2"),
    ] {
        let (requests, _receiver) = mpsc::channel(1);
        let (stop, _stopped) = watch::channel(false);
        room.attach(
            cid,
            ConnectorPeer {
                generation: generation.into(),
                sender: requests,
                stop,
            },
        );
        assert_eq!(room.connector_peer().unwrap().generation, generation);
    }
    room.detach("first", "first-1");
    assert_eq!(room.connector_peer().unwrap().generation, "first-2");
    room.detach("first", "first-2");
    assert_eq!(room.connector_peer().unwrap().generation, "second-1");
}
