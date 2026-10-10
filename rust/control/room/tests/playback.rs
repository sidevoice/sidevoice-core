//! Replies sent to calls, and what the calls report about them.
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::{pull_room, report};
use crate::control::room::replay::MAX_REPLAY_RECORDS;
use crate::control::room::Room;

fn join(room: &Room, thread: &str) -> (String, mpsc::Receiver<Value>) {
    let (events, received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    room.select(&sid, thread).unwrap();
    (sid, received)
}

fn revision(room: &Room, sid: &str) -> u64 {
    room.snapshot(Some(sid))["room"]["revision"]
        .as_u64()
        .unwrap()
}

fn publish(room: &Room, sid: &str, uid: &str) -> Value {
    room.publish(
        &json!({"session_id":sid,"thread_id":"thread","revision":revision(room, sid),
            "utterance_id":uid,"text":"A reply of thirty characters."}),
    )
}

fn row(room: &Room, row_id: &str) -> (String, Option<String>, Option<u64>) {
    let inner = room.inner.lock().unwrap();
    let row = inner.journal.find(row_id).unwrap();
    (row.status.clone(), row.reason.clone(), row.heard_chars)
}

/// A report a call sends: its status, reason and heard characters.
type Report<'a> = (&'a str, Option<&'a str>, Option<u64>);

fn room_on_thread() -> (tempfile::TempDir, Room) {
    let (directory, room, _) = pull_room(&["connector"]);
    room.register("connector", &json!({"thread":"thread","harness":"codex"}))
        .unwrap();
    (directory, room)
}

#[test]
fn what_a_call_reports_is_what_the_reply_row_shows() {
    let (_directory, room) = room_on_thread();
    let (sid, _events) = join(&room, "thread");
    let cases: [(&str, &[Report], Report); 6] = [
        (
            "heard",
            &[("playing", None, None), ("heard", None, None)],
            ("playback_finished", None, None),
        ),
        (
            "cut",
            &[("playing", None, None), ("interrupted", None, Some(12))],
            ("interrupted", Some("user_interrupted"), Some(12)),
        ),
        (
            "skipped",
            &[
                ("playing", None, None),
                ("interrupted", Some("user_skipped"), Some(3)),
            ],
            ("interrupted", Some("user_skipped"), Some(3)),
        ),
        (
            "dropped",
            &[("unplayed", None, None)],
            ("interrupted", Some("newer_turn"), None),
        ),
        (
            "away",
            &[("unplayed", Some("unheard"), None)],
            ("interrupted", Some("unheard"), None),
        ),
        (
            "broken",
            &[("playing", None, None), ("failed", None, None)],
            ("failed", Some("playback_failed"), None),
        ),
    ];
    for (uid, reports, (status, reason, heard)) in cases {
        assert_eq!(publish(&room, &sid, uid)["status"], "queued");
        for (report, reason, heard) in reports {
            room.playback(&sid, uid, report, *reason, *heard, &Value::Null)
                .unwrap();
        }
        assert_eq!(
            row(&room, &format!("{sid}:voice:{uid}")),
            (status.into(), reason.map(str::to_owned), heard),
            "{uid}"
        );
    }
}

#[test]
fn a_report_on_an_ended_reply_changes_nothing_and_a_bad_one_is_refused() {
    let (_directory, room) = room_on_thread();
    let (sid, _events) = join(&room, "thread");
    publish(&room, &sid, "r");
    report(&room, &sid, "r", "heard");
    report(&room, &sid, "r", "interrupted");
    assert_eq!(row(&room, &format!("{sid}:voice:r")).0, "playback_finished");
    let refused = |uid: &str, status: &str, reason: Option<&str>| {
        room.playback(&sid, uid, status, reason, None, &Value::Null)
            .unwrap_err()
    };
    assert_eq!(refused("r", "paused", None).key, "room.receipt_invalid");
    assert_eq!(
        refused("r", "interrupted", Some("bored")).key,
        "room.receipt_invalid"
    );
    let unknown = refused("unknown", "playing", None);
    assert_eq!((unknown.status, unknown.key), (409, "room.stale_utterance"));
}

/// A replay shares its original's journal row; only the original's calls decide that row's
/// status. Replay records outnumber the original so that a lookup by row that could land on
/// a replay almost surely does, whatever the map's iteration order.
#[test]
fn replays_never_decide_their_original_row_status() {
    for _ in 0..4 {
        let (_directory, room) = room_on_thread();
        let (listener, mut listener_events) = join(&room, "thread");
        let (other, _other_events) = join(&room, "thread");
        assert_eq!(publish(&room, &listener, "original")["status"], "queued");
        report(&room, &listener, "original", "playing");
        report(&room, &listener, "original", "heard");
        let history_id = format!("{listener}:voice:original");
        assert_eq!(row(&room, &history_id).0, "playback_finished");
        while listener_events.try_recv().is_ok() {}
        for index in 0..MAX_REPLAY_RECORDS {
            let uid = format!("replay-{index}");
            room.replay_one(&listener, &history_id, &uid).unwrap();
            let sent = listener_events.try_recv().unwrap();
            assert_eq!(sent["type"], "voice-reply");
            assert_eq!(sent["data"]["replay"], true);
            assert_eq!(sent["data"]["requested"], true);
            assert_eq!(sent["data"]["history_id"], history_id.as_str());
        }
        assert_eq!(
            room.replay_one(&listener, &history_id, "one-more")
                .unwrap_err()
                .key,
            "room.replay_full"
        );
        report(&room, &listener, "replay-0", "interrupted");
        assert_eq!(row(&room, &history_id).0, "playback_finished");

        // The other call still holds the original; its new turn leaves the original's row as it is.
        room.begin_turn(&other, "turn").unwrap();
        assert_eq!(row(&room, &history_id).0, "playback_finished");
    }
}
