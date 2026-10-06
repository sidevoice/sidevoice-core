//! Unit tests of the turn owner: the recognition queue, browser events and catch-up slices.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use base64::Engine;
use serde_json::json;
use tokio::sync::mpsc;

use super::{
    audio_idle_timeout,
    catchup::{slice_header, started_at, Catchup, SliceError},
    events,
    queue::MAX_RECOGNITION_QUEUE,
    TurnOwner,
};
use crate::{
    control::room::{Room, VoiceTurn},
    pipeline::CallFrame,
    server::media::{call::CallMedia, recognition::SttFailure},
    storage::PrivateDir,
};

#[tokio::test]
async fn recognition_queue_is_bounded_and_drained_on_close() {
    let directory = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(directory.path().join("private")).unwrap();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let (events, _received) = mpsc::channel(64);
    let sid = room
        .join("device".into(), "en".into(), events.clone())
        .unwrap();
    let mut settings = crate::models::default_settings(None, None);
    settings.turn_end_mode = "timer".into();
    let (media, _frames, _focus) = CallMedia::start(&settings).unwrap();
    let mut owner = TurnOwner::new(media, room.clone(), settings, sid.clone(), events, dir);
    for _ in 0..MAX_RECOGNITION_QUEUE + 3 {
        owner.frame(CallFrame::Audio(vec![0; 2048])).await;
        owner.frame(CallFrame::Started).await;
        owner.frame(CallFrame::Stopped { stop_secs: 0.5 }).await;
    }
    assert!(owner.active.is_some());
    assert_eq!(owner.queue.len(), MAX_RECOGNITION_QUEUE - 1);
    owner.close().await;
    assert!(owner.active.is_none() && owner.queue.is_empty());
    assert!(room.history(None)["messages"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// A turn opened by a joined browser, pinned to revision 4 and the given focus.
fn turn(thread: Option<&str>) -> VoiceTurn {
    let directory = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
    let (events, _received) = mpsc::channel(4);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    let mut turn = room.begin_turn(&sid).unwrap();
    turn.revision = 4;
    turn.thread_id = thread.map(str::to_owned);
    turn
}

#[test]
fn user_turn_events_keep_their_wire_shape() {
    let focused = turn(Some("thread"));
    assert_eq!(
        events::started(&focused).to_string(),
        r#"{"phase":"started","revision":4,"thread_id":"thread"}"#
    );
    assert_eq!(
        events::cancelled(&focused, "").to_string(),
        r#"{"phase":"cancelled","revision":4,"thread_id":"thread","text":""}"#
    );
    assert_eq!(
        events::merged(&focused, "hi").to_string(),
        r#"{"phase":"cancelled","revision":4,"thread_id":"thread","text":"hi","merged":true}"#
    );
    assert_eq!(
        events::finished(&turn(None), "hi").to_string(),
        r#"{"phase":"finished","revision":4,"thread_id":null,"text":"hi"}"#
    );
}

#[test]
fn recognition_failures_map_to_their_message_keys() {
    assert_eq!(
        events::failure_key(SttFailure::Timeout, false),
        "voice.transcription_timeout"
    );
    assert_eq!(
        events::failure_key(SttFailure::Device, false),
        "voice.transcription_failed"
    );
    assert_eq!(
        events::failure_key(SttFailure::Provider, false),
        "voice.transcription_failed"
    );
    assert_eq!(
        events::failure_key(SttFailure::Timeout, true),
        "voice.catchup_transcription_failed"
    );
}

fn slice(seq: u64, audio: &[u8], last: bool) -> serde_json::Value {
    json!({"seq":seq,"sample_rate":8_000,"final":last,
        "audio_base64":base64::engine::general_purpose::STANDARD.encode(audio)})
}

#[test]
fn catchup_slices_append_in_order_until_the_final_one() {
    let mut catchup = Catchup::begin(&json!({"truncated":true}), 8_000, 0);
    assert_eq!(
        catchup.append(&slice(0, &[1, 2], false), 8_000, 0),
        Ok(false)
    );
    assert_eq!(
        catchup.append(&slice(2, &[3, 4], false), 8_000, 2),
        Err(SliceError::Invalid)
    );
    assert_eq!(
        catchup.append(&slice(1, &[3, 4], false), 16_000, 1),
        Err(SliceError::Invalid)
    );
    assert_eq!(
        catchup.append(
            &json!({"seq":1,"sample_rate":8_000,"audio_base64":"%"}),
            8_000,
            1
        ),
        Err(SliceError::Invalid)
    );
    assert_eq!(
        catchup.append(&json!({"seq":1}), 8_000, 1),
        Err(SliceError::Invalid)
    );
    assert_eq!(catchup.append(&slice(1, &[3, 4], true), 8_000, 1), Ok(true));
}

#[test]
fn catchup_refuses_oversized_slices_and_recordings() {
    let mut catchup = Catchup::begin(&json!({}), 8_000, 0);
    let oversized = vec![0; 128 * 1024 + 2];
    assert_eq!(
        catchup.append(&slice(0, &oversized, false), 8_000, 0),
        Err(SliceError::TooLong)
    );
    let limit = 35 * 8_000 * 2;
    let chunk = vec![0; 100_000];
    let mut seq = 0;
    let mut total = 0;
    while total + chunk.len() <= limit {
        assert_eq!(
            catchup.append(&slice(seq, &chunk, false), 8_000, seq),
            Ok(false)
        );
        seq += 1;
        total += chunk.len();
    }
    assert_eq!(
        catchup.append(&slice(seq, &chunk, false), 8_000, seq),
        Err(SliceError::TooLong)
    );
}

#[test]
fn catchup_header_and_start_time_are_validated() {
    assert_eq!(
        slice_header(&json!({"seq":3,"sample_rate":48_000})),
        Some((48_000, 3))
    );
    assert_eq!(slice_header(&json!({"seq":3,"sample_rate":7_999})), None);
    assert_eq!(slice_header(&json!({"seq":3,"sample_rate":48_001})), None);
    assert_eq!(slice_header(&json!({"sample_rate":16_000})), None);
    let now = 10_000_000;
    assert_eq!(
        started_at(&json!({"started_at":now as f64}), now),
        Some(now)
    );
    assert_eq!(
        started_at(&json!({"started_at":(now - 3_600_000) as f64}), now),
        Some(now - 3_600_000)
    );
    assert_eq!(
        started_at(&json!({"started_at":(now - 3_600_001) as f64}), now),
        None
    );
    assert_eq!(
        started_at(&json!({"started_at":(now + 60_000) as f64}), now),
        Some(now + 60_000)
    );
    assert_eq!(
        started_at(&json!({"started_at":(now + 60_001) as f64}), now),
        None
    );
    assert_eq!(started_at(&json!({}), now), None);
}

#[tokio::test]
async fn an_open_turn_closes_when_its_audio_stops() {
    let directory = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(directory.path().join("private")).unwrap();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let (events, _received) = mpsc::channel(64);
    let sid = room
        .join("device".into(), "en".into(), events.clone())
        .unwrap();
    let settings = crate::models::default_settings(None, None);
    let (media, _frames, _focus) = CallMedia::start(&settings).unwrap();
    let mut owner = TurnOwner::new(media, room.clone(), settings, sid, events, dir);
    owner.idle_timeout = Some(Duration::from_millis(30));

    // No turn open: a quiet microphone is nothing to close.
    assert!(owner.deadline().is_none());
    owner.frame(CallFrame::Audio(vec![0; 640])).await;
    owner.frame(CallFrame::Started).await;
    assert!(owner.speaking.is_some());
    owner.frame(CallFrame::Audio(vec![0; 640])).await;
    let deadline = owner.deadline().expect("an open turn waits for its audio");
    assert!(deadline <= Instant::now() + Duration::from_millis(30));

    // Audio still arriving keeps the turn open.
    owner.expired().await;
    assert!(owner.speaking.is_some());

    tokio::time::sleep(Duration::from_millis(40)).await;
    owner.expired().await;
    assert!(owner.speaking.is_none(), "the idle turn was closed");
    assert!(owner.active.is_some(), "and handed to recognition");
    owner.close().await;
}

#[test]
fn audio_idle_timeout_reads_the_environment() {
    assert_eq!(audio_idle_timeout(None), Some(Duration::from_secs(5)));
    assert_eq!(
        audio_idle_timeout(Some("2.5")),
        Some(Duration::from_millis(2500))
    );
    assert_eq!(audio_idle_timeout(Some("0")), None);
    assert_eq!(audio_idle_timeout(Some("-3")), None);
    assert_eq!(
        audio_idle_timeout(Some("later")),
        Some(Duration::from_secs(5))
    );
}
