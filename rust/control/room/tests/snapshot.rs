use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::control::room::Room;
use crate::storage::PrivateDir;

#[test]
fn room_stats_show_the_call_transcription() {
    let directory = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
    let (events, _received) = mpsc::channel(4);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    assert_eq!(
        room.snapshot(Some(&sid))["call"]["transcription"],
        Value::Null
    );
    let transcription = json!({"place":"device","model":"whisper-base","accelerator":"webgpu"});
    room.set_transcription(&sid, transcription.clone());
    let stats = room.snapshot(Some(&sid));
    assert_eq!(stats["call"]["transcription"], transcription);
    assert_eq!(stats["clients"][0]["transcription"], transcription);
}
