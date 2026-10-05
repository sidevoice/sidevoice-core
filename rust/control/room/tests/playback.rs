use serde_json::json;
use tokio::sync::mpsc;

use super::support::pull_room;
use crate::control::room::playback::MAX_PENDING;
use crate::control::room::Room;

fn join(room: &Room, thread: &str) -> (String, mpsc::Receiver<serde_json::Value>) {
    let (events, received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, thread).unwrap();
    (sid, received)
}

fn row_status(room: &Room, row_id: &str) -> String {
    room.inner
        .lock()
        .unwrap()
        .journal
        .find(row_id)
        .unwrap()
        .status
        .clone()
}

/// A replay shares its original's journal row; only the original's calls decide that row's
/// status. Replay records outnumber the original so that a lookup by row that could land on
/// a replay almost surely does, whatever the map's iteration order.
#[test]
fn replays_never_decide_their_original_row_status() {
    for _ in 0..4 {
        let (_directory, room, _) = pull_room(&["connector"]);
        room.register("connector", &json!({"thread":"thread","harness":"codex"}))
            .unwrap();
        let (listener, _listener_events) = join(&room, "thread");
        let (other, _other_events) = join(&room, "thread");
        let revision = |sid: &str| {
            room.snapshot(Some(sid))["room"]["revision"]
                .as_u64()
                .unwrap()
        };
        let reply = room.publish(
            &json!({"session_id":listener,"thread_id":"thread","revision":revision(&listener),
                "utterance_id":"original","text":"Original reply"}),
            false,
        );
        assert_eq!(reply["status"], "queued");
        room.receipt(&listener, "original", revision(&listener), "playing")
            .unwrap();
        room.receipt(
            &listener,
            "original",
            revision(&listener),
            "playback_finished",
        )
        .unwrap();
        let history_id = format!("{listener}:voice:original");
        assert_eq!(row_status(&room, &history_id), "playback_finished");
        for index in 0..MAX_PENDING - 2 {
            room.replay_one(&listener, &history_id, &format!("replay-{index}"))
                .unwrap();
        }
        assert_eq!(row_status(&room, &history_id), "playback_finished");

        // The other call still holds the original; its new turn re-syncs the original's row.
        room.begin_turn(&other).unwrap();
        assert_eq!(row_status(&room, &history_id), "playback_finished");
    }
}
