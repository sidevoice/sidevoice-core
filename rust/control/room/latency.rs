//! Latency observations of calls: the public record types, and the room's operations that take
//! and hand out those records.
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::Value;

use super::{Inner, Room};

/// One process-wide monotonic origin for Room and call observations. T6 consumes
/// these microseconds directly; no wall-clock subtraction enters latency.
pub fn latency_now_micros() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatencyEvent {
    SpeechEnd,
    TurnClosed,
    Transcript,
    TranscriptDelivered,
    Queued,
    DeliveryAccepted,
    Read,
    ReplyReceived,
    ReplyDispatched,
    AudioReady,
    AudioDispatched,
    PlayingReceipt,
}

#[derive(Clone, Debug)]
pub struct LatencyMark {
    pub session_id: String,
    pub thread_id: String,
    pub revision: u64,
    pub utterance_id: Option<String>,
    pub event: LatencyEvent,
    pub at_micros: u64,
}

#[derive(Clone, Debug)]
pub struct LatencyDuration {
    pub name: String,
    pub milliseconds: f64,
}

#[derive(Clone, Debug)]
pub struct LatencyReply {
    pub session_id: String,
    pub thread_id: String,
    pub revision: u64,
    pub utterance_id: String,
    pub status: String,
    pub input_ms: Vec<LatencyDuration>,
    pub provider_ms: Vec<LatencyDuration>,
    pub browser_ms: Vec<LatencyDuration>,
}

impl Room {
    pub fn latency_mark(
        &self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: Option<&str>,
        event: LatencyEvent,
        at_micros: u64,
    ) {
        let mut inner = self.inner.lock().expect("room lock");
        inner.mark_latency(sid, thread, revision, uid, event, at_micros);
    }
    pub fn latency_duration(
        &self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: Option<&str>,
        name: &str,
        milliseconds: f64,
    ) {
        let mut inner = self.inner.lock().expect("room lock");
        inner.record_latency_duration(sid, thread, revision, uid, name, milliseconds);
    }
    /// Clone only the authenticated call's bounded records for T6's borrowed snapshot API.
    pub fn latency_records(
        &self,
        sid: &str,
        device: &str,
    ) -> Option<(String, Vec<LatencyMark>, Vec<LatencyReply>)> {
        let inner = self.inner.lock().expect("room lock");
        let language = inner
            .browsers
            .get(sid)
            .filter(|browser| browser.device == device)?
            .language
            .clone();
        let (marks, replies) = inner.latency.records(sid);
        Some((language, marks, replies))
    }
    pub fn latency_browser(&self, sid: &str, uid: &str, timings: &Value) {
        let reply = self
            .inner
            .lock()
            .expect("room lock")
            .latency
            .reply_turn(sid, uid);
        let Some((thread, revision)) = reply else {
            return;
        };
        for name in [
            "audio_received_to_playback_scheduled_ms",
            "turn_finished_event_to_playback_scheduled_ms",
        ] {
            if let Some(milliseconds) = timings.get(name).and_then(Value::as_f64) {
                self.latency_duration(sid, &thread, revision, Some(uid), name, milliseconds);
            }
        }
    }
}

impl Inner {
    /// Record a duration a call measured, in milliseconds, if it is a plausible one and the call is still in the room.
    pub(super) fn record_latency_duration(
        &mut self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: Option<&str>,
        name: &str,
        milliseconds: f64,
    ) {
        if milliseconds.is_finite()
            && (0.0..=3_600_000.0).contains(&milliseconds)
            && self.browsers.contains(sid)
        {
            self.latency
                .record_duration(sid, thread, revision, uid, name, milliseconds);
        }
    }
    /// Mark a latency event of a call that is still in the room.
    pub(super) fn mark_latency(
        &mut self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: Option<&str>,
        event: LatencyEvent,
        at_micros: u64,
    ) {
        if self.browsers.contains(sid) {
            self.latency
                .mark(sid, thread, revision, uid, event, at_micros);
        }
    }
    /// Open the latency row of a reply spoken to a call that is still in the room.
    pub(super) fn register_latency_reply(
        &mut self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: &str,
        status: &str,
    ) {
        if self.browsers.contains(sid) {
            self.latency
                .register_reply(sid, thread, revision, uid, status);
        }
    }
}
