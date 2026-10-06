//! Latency bookkeeping: per-call marks, reply rows and input durations, and their telemetry.
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::{json, Value};

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
    SynthesisStarted,
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
    pub synthesis_attempt: u32,
    pub input_ms: Vec<LatencyDuration>,
    pub provider_ms: Vec<LatencyDuration>,
    pub browser_ms: Vec<LatencyDuration>,
}

pub(super) const MAX_LATENCY_MARKS: usize = 2048;
pub(super) const MAX_LATENCY_REPLIES: usize = 128;

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
        mark_latency(&mut inner, sid, thread, revision, uid, event, at_micros);
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
        if !milliseconds.is_finite() || !(0.0..=3_600_000.0).contains(&milliseconds) {
            return;
        }
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.browsers.contains_key(sid) {
            return;
        }
        let duration = LatencyDuration {
            name: name.to_owned(),
            milliseconds: (milliseconds * 100.0).round() / 100.0,
        };
        if let Some(uid) = uid {
            let Some(replies) = inner.latency_replies.get_mut(sid) else {
                return;
            };
            let Some(reply) = replies
                .iter_mut()
                .find(|reply| reply.utterance_id == uid && reply.thread_id == thread)
            else {
                return;
            };
            let target = if matches!(
                name,
                "request_to_headers_ms" | "request_to_first_chunk_ms" | "request_to_complete_ms"
            ) {
                &mut reply.provider_ms
            } else if matches!(
                name,
                "audio_received_to_playback_scheduled_ms"
                    | "turn_finished_event_to_playback_scheduled_ms"
            ) {
                &mut reply.browser_ms
            } else {
                return;
            };
            if let Some(existing) = target.iter_mut().find(|item| item.name == name) {
                *existing = duration;
            } else {
                target.push(duration);
            }
        } else {
            if !matches!(
                name,
                "audio_ms"
                    | "endpoint_silence_ms"
                    | "recognition_ms"
                    | "request_to_transcript_ms"
                    | "speech_end_to_transcript_ms"
                    | "transcript_to_delivery_ms"
            ) {
                return;
            }
            let target = inner
                .latency_input
                .entry((sid.into(), thread.into(), revision))
                .or_default();
            if let Some(existing) = target.iter_mut().find(|item| item.name == name) {
                *existing = duration.clone();
            } else {
                target.push(duration.clone());
            }
            if let Some(replies) = inner.latency_replies.get_mut(sid) {
                for reply in replies
                    .iter_mut()
                    .filter(|reply| reply.thread_id == thread && reply.revision == revision)
                {
                    if let Some(existing) = reply.input_ms.iter_mut().find(|item| item.name == name)
                    {
                        *existing = duration.clone();
                    } else {
                        reply.input_ms.push(duration.clone());
                    }
                }
            }
            // The input index only serves recent reply rows; a long call must not retain every turn.
            while inner
                .latency_input
                .keys()
                .filter(|(session, _, _)| session == sid)
                .count()
                > MAX_LATENCY_REPLIES
            {
                let oldest = inner
                    .latency_input
                    .keys()
                    .filter(|(session, _, _)| session == sid)
                    .min_by_key(|(_, _, revision)| revision)
                    .cloned();
                if let Some(oldest) = oldest {
                    inner.latency_input.remove(&oldest);
                }
            }
        }
        let stage = match name {
            "request_to_transcript_ms" => Some("request_to_transcript"),
            "request_to_complete_ms" => Some("provider_synthesis"),
            "audio_received_to_playback_scheduled_ms" => Some("audio_received_to_playback"),
            _ => None,
        };
        if let (Some(stage), Some(telemetry)) = (stage, inner.telemetry.as_ref()) {
            telemetry.try_observe(
                stage,
                milliseconds,
                &json!({
                    "sidevoice.session_id": sid, "sidevoice.thread_id": thread,
                    "sidevoice.turn_revision": revision, "sidevoice.utterance_id": uid,
                }),
            );
        }
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
        Some((
            language,
            inner
                .latency_marks
                .get(sid)
                .map_or_else(Vec::new, |marks| marks.iter().cloned().collect()),
            inner
                .latency_replies
                .get(sid)
                .map_or_else(Vec::new, |rows| rows.iter().cloned().collect()),
        ))
    }
    pub fn latency_browser(&self, sid: &str, uid: &str, timings: &Value) {
        let reply = self
            .inner
            .lock()
            .expect("room lock")
            .latency_replies
            .get(sid)
            .and_then(|rows| rows.iter().find(|row| row.utterance_id == uid))
            .map(|row| (row.thread_id.clone(), row.revision));
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

pub(super) fn mark_latency(
    inner: &mut Inner,
    sid: &str,
    thread: &str,
    revision: u64,
    uid: Option<&str>,
    event: LatencyEvent,
    at_micros: u64,
) {
    if thread.is_empty() || !inner.browsers.contains_key(sid) {
        return;
    }
    let marks = inner.latency_marks.entry(sid.to_owned()).or_default();
    if marks.iter().any(|mark| {
        mark.thread_id == thread
            && mark.revision == revision
            && mark.utterance_id.as_deref() == uid
            && mark.event == event
    }) {
        return;
    }
    marks.push_back(LatencyMark {
        session_id: sid.into(),
        thread_id: thread.into(),
        revision,
        utterance_id: uid.map(str::to_owned),
        event,
        at_micros,
    });
    while marks.len() > MAX_LATENCY_MARKS {
        marks.pop_front();
    }
    if let Some(uid) = uid {
        if let Some(reply) = inner
            .latency_replies
            .get_mut(sid)
            .and_then(|rows| rows.iter_mut().find(|row| row.utterance_id == uid))
        {
            if event == LatencyEvent::SynthesisStarted {
                reply.synthesis_attempt += 1;
                reply.provider_ms.clear();
                reply.browser_ms.clear();
            }
        }
    }
    let stages: &[(&str, LatencyEvent, bool)] = match event {
        LatencyEvent::TurnClosed => &[("endpoint_silence", LatencyEvent::SpeechEnd, false)],
        LatencyEvent::Transcript => &[("recognition", LatencyEvent::TurnClosed, false)],
        LatencyEvent::TranscriptDelivered => {
            &[("transcript_to_delivery", LatencyEvent::Transcript, false)]
        }
        LatencyEvent::Read => &[("delivery_to_read", LatencyEvent::DeliveryAccepted, false)],
        LatencyEvent::ReplyReceived => &[
            ("read_to_reply", LatencyEvent::Read, false),
            ("input_queued_to_reply", LatencyEvent::Queued, false),
        ],
        LatencyEvent::SynthesisStarted => {
            &[("reply_to_synthesis", LatencyEvent::ReplyReceived, true)]
        }
        _ => &[],
    };
    if let (Some(telemetry), Some(marks)) = (inner.telemetry.as_ref(), inner.latency_marks.get(sid))
    {
        for &(stage, start_event, same_uid) in stages {
            let started = marks.iter().rev().find(|mark| {
                mark.thread_id == thread
                    && mark.revision == revision
                    && mark.event == start_event
                    && mark.utterance_id.as_deref() == if same_uid { uid } else { None }
            });
            if let Some(milliseconds) =
                started.and_then(|mark| at_micros.checked_sub(mark.at_micros))
            {
                telemetry.try_observe(
                    stage,
                    milliseconds as f64 / 1000.0,
                    &json!({
                        "sidevoice.session_id": sid, "sidevoice.thread_id": thread,
                        "sidevoice.turn_revision": revision, "sidevoice.utterance_id": uid,
                    }),
                );
            }
        }
    }
}

pub(super) fn register_latency_reply(
    inner: &mut Inner,
    sid: &str,
    thread: &str,
    revision: u64,
    uid: &str,
    status: &str,
) {
    if !inner.browsers.contains_key(sid) {
        return;
    }
    let input = inner
        .latency_input
        .get(&(sid.into(), thread.into(), revision))
        .cloned()
        .unwrap_or_default();
    let rows = inner.latency_replies.entry(sid.into()).or_default();
    if rows.iter().any(|row| row.utterance_id == uid) {
        return;
    }
    rows.push_back(LatencyReply {
        session_id: sid.into(),
        thread_id: thread.into(),
        revision,
        utterance_id: uid.into(),
        status: status.into(),
        synthesis_attempt: 0,
        input_ms: input,
        provider_ms: Vec::new(),
        browser_ms: Vec::new(),
    });
    while rows.len() > MAX_LATENCY_REPLIES {
        rows.pop_front();
    }
    mark_latency(
        inner,
        sid,
        thread,
        revision,
        Some(uid),
        LatencyEvent::ReplyReceived,
        latency_now_micros(),
    );
}

pub(super) fn status_latency_reply(inner: &mut Inner, sid: &str, uid: &str, status: &str) {
    if let Some(reply) = inner
        .latency_replies
        .get_mut(sid)
        .and_then(|rows| rows.iter_mut().find(|row| row.utterance_id == uid))
    {
        reply.status = status.into();
    }
}
