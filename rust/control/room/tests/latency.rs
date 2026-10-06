use tokio::sync::mpsc;

use crate::control::room::latency::{register_latency_reply, MAX_LATENCY_REPLIES};
use crate::control::room::Room;
use crate::storage::PrivateDir;

#[test]
fn latency_input_keeps_only_recent_turns_and_seeds_recent_reply() {
    let directory = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
    let (events, _received) = mpsc::channel(4);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    for revision in 1..=MAX_LATENCY_REPLIES as u64 + 3 {
        room.latency_duration(&sid, "thread", revision, None, "audio_ms", revision as f64);
    }
    let mut inner = room.inner.lock().unwrap();
    assert_eq!(inner.latency_input.len(), MAX_LATENCY_REPLIES);
    assert!(!inner
        .latency_input
        .contains_key(&(sid.clone(), "thread".into(), 1)));
    let newest = MAX_LATENCY_REPLIES as u64 + 3;
    register_latency_reply(&mut inner, &sid, "thread", newest, "reply", "queued");
    assert_eq!(
        inner.latency_replies[&sid].back().unwrap().input_ms[0].milliseconds,
        newest as f64
    );
}
