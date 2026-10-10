//! Latency bookkeeping for each call: its marks, reply rows and input durations, and the
//! telemetry stages they complete.
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde_json::json;

use super::latency::{
    latency_now_micros, LatencyDuration, LatencyEvent, LatencyMark, LatencyReply,
};
use crate::control::telemetry::Telemetry;

const MAX_LATENCY_MARKS: usize = 2048;
pub(super) const MAX_LATENCY_REPLIES: usize = 128;

const PROVIDER_DURATIONS: [&str; 3] = [
    "request_to_headers_ms",
    "request_to_first_chunk_ms",
    "request_to_complete_ms",
];
const BROWSER_DURATIONS: [&str; 2] = [
    "audio_received_to_playback_scheduled_ms",
    "turn_finished_event_to_playback_scheduled_ms",
];
const INPUT_DURATIONS: [&str; 6] = [
    "audio_ms",
    "endpoint_silence_ms",
    "recognition_ms",
    "request_to_transcript_ms",
    "speech_end_to_transcript_ms",
    "transcript_to_delivery_ms",
];

type TurnKey = (String, String, u64);

pub(super) struct LatencyLog {
    telemetry: Option<Arc<Telemetry>>,
    marks: HashMap<String, VecDeque<LatencyMark>>,
    replies: HashMap<String, VecDeque<LatencyReply>>,
    input: HashMap<TurnKey, Vec<LatencyDuration>>,
}
impl LatencyLog {
    pub(super) fn new(telemetry: Option<Arc<Telemetry>>) -> Self {
        Self {
            telemetry,
            marks: HashMap::new(),
            replies: HashMap::new(),
            input: HashMap::new(),
        }
    }
    pub(super) fn open(&mut self, sid: &str) {
        self.marks.insert(sid.to_owned(), VecDeque::new());
        self.replies.insert(sid.to_owned(), VecDeque::new());
    }
    pub(super) fn close(&mut self, sid: &str) {
        self.marks.remove(sid);
        self.replies.remove(sid);
        self.input.retain(|(session, _, _), _| session != sid);
    }
    pub(super) fn records(&self, sid: &str) -> (Vec<LatencyMark>, Vec<LatencyReply>) {
        (
            self.marks
                .get(sid)
                .map_or_else(Vec::new, |marks| marks.iter().cloned().collect()),
            self.replies
                .get(sid)
                .map_or_else(Vec::new, |rows| rows.iter().cloned().collect()),
        )
    }
    /// The thread and revision of the turn a reply answered.
    pub(super) fn reply_turn(&self, sid: &str, uid: &str) -> Option<(String, u64)> {
        self.reply(sid, uid)
            .map(|row| (row.thread_id.clone(), row.revision))
    }
    fn reply(&self, sid: &str, uid: &str) -> Option<&LatencyReply> {
        self.replies
            .get(sid)
            .and_then(|rows| rows.iter().find(|row| row.utterance_id == uid))
    }
    fn reply_mut(&mut self, sid: &str, uid: &str) -> Option<&mut LatencyReply> {
        self.replies
            .get_mut(sid)
            .and_then(|rows| rows.iter_mut().find(|row| row.utterance_id == uid))
    }
    pub(super) fn set_reply_status(&mut self, sid: &str, uid: &str, status: &str) {
        if let Some(reply) = self.reply_mut(sid, uid) {
            reply.status = status.into();
        }
    }

    /// Record an event once, and observe each stage it ends. The call must be in the room.
    pub(super) fn mark(
        &mut self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: Option<&str>,
        event: LatencyEvent,
        at_micros: u64,
    ) {
        if thread.is_empty() {
            return;
        }
        let marks = self.marks.entry(sid.to_owned()).or_default();
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
        self.observe_stages(sid, thread, revision, uid, event, at_micros);
    }
    fn observe_stages(
        &self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: Option<&str>,
        event: LatencyEvent,
        at_micros: u64,
    ) {
        let (Some(telemetry), Some(marks)) = (self.telemetry.as_ref(), self.marks.get(sid)) else {
            return;
        };
        for &(stage, start_event, same_uid) in stages_ended_by(event) {
            let started = marks.iter().rev().find(|mark| {
                mark.thread_id == thread
                    && mark.revision == revision
                    && mark.event == start_event
                    && mark.utterance_id.as_deref() == if same_uid { uid } else { None }
            });
            if let Some(started) = started {
                telemetry.stage(
                    sid,
                    thread,
                    revision,
                    stage,
                    started.at_micros,
                    at_micros,
                    latency_now_micros(),
                    &json!({"sidevoice.utterance_id": uid,
                        "sidevoice.reply_revision": uid.map(|_| revision)}),
                );
            }
        }
    }

    /// Open the latency row of a reply spoken to a call, seeded with the durations its input
    /// turn already measured. The call must be in the room.
    pub(super) fn register_reply(
        &mut self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: &str,
        status: &str,
    ) {
        let input = self
            .input
            .get(&(sid.into(), thread.into(), revision))
            .cloned()
            .unwrap_or_default();
        let rows = self.replies.entry(sid.into()).or_default();
        if rows.iter().any(|row| row.utterance_id == uid) {
            return;
        }
        rows.push_back(LatencyReply {
            session_id: sid.into(),
            thread_id: thread.into(),
            revision,
            utterance_id: uid.into(),
            status: status.into(),
            input_ms: input,
            provider_ms: Vec::new(),
            browser_ms: Vec::new(),
        });
        while rows.len() > MAX_LATENCY_REPLIES {
            rows.pop_front();
        }
        self.mark(
            sid,
            thread,
            revision,
            Some(uid),
            LatencyEvent::ReplyReceived,
            latency_now_micros(),
        );
    }

    /// Record a named duration, of a reply when `uid` is given and of the input turn otherwise,
    /// and observe it if it is a telemetry stage. Unknown names and replies are ignored. The
    /// call must be in the room.
    pub(super) fn record_duration(
        &mut self,
        sid: &str,
        thread: &str,
        revision: u64,
        uid: Option<&str>,
        name: &str,
        milliseconds: f64,
    ) {
        let duration = LatencyDuration {
            name: name.to_owned(),
            milliseconds: (milliseconds * 100.0).round() / 100.0,
        };
        let recorded = match uid {
            Some(uid) => self.record_reply_duration(sid, thread, uid, duration),
            None => self.record_input_duration(sid, thread, revision, duration),
        };
        if !recorded {
            return;
        }
        let stage = match name {
            "request_to_transcript_ms" => Some("request_to_transcript"),
            "request_to_complete_ms" => Some("provider_synthesis"),
            "audio_received_to_playback_scheduled_ms" => Some("audio_received_to_playback"),
            _ => None,
        };
        if let (Some(stage), Some(telemetry)) = (stage, self.telemetry.as_ref()) {
            // The browser's playback span is the browser's own: the room records its histogram only.
            if stage == "audio_received_to_playback" {
                let values = json!({"sidevoice.utterance_id": uid, "sidevoice.thread_id": thread,
                    "sidevoice.reply_revision": revision});
                telemetry.observe(sid, stage, milliseconds, &values);
            } else {
                let values = json!({"sidevoice.utterance_id": uid});
                telemetry.duration_stage(sid, thread, revision, stage, milliseconds, &values);
            }
        }
    }
    fn record_reply_duration(
        &mut self,
        sid: &str,
        thread: &str,
        uid: &str,
        duration: LatencyDuration,
    ) -> bool {
        let Some(reply) = self.replies.get_mut(sid).and_then(|replies| {
            replies
                .iter_mut()
                .find(|reply| reply.utterance_id == uid && reply.thread_id == thread)
        }) else {
            return false;
        };
        let name = duration.name.as_str();
        let target = if PROVIDER_DURATIONS.contains(&name) {
            &mut reply.provider_ms
        } else if BROWSER_DURATIONS.contains(&name) {
            &mut reply.browser_ms
        } else {
            return false;
        };
        upsert(target, duration);
        true
    }
    fn record_input_duration(
        &mut self,
        sid: &str,
        thread: &str,
        revision: u64,
        duration: LatencyDuration,
    ) -> bool {
        if !INPUT_DURATIONS.contains(&duration.name.as_str()) {
            return false;
        }
        upsert(
            self.input
                .entry((sid.into(), thread.into(), revision))
                .or_default(),
            duration.clone(),
        );
        if let Some(replies) = self.replies.get_mut(sid) {
            for reply in replies
                .iter_mut()
                .filter(|reply| reply.thread_id == thread && reply.revision == revision)
            {
                upsert(&mut reply.input_ms, duration.clone());
            }
        }
        // The input index only serves recent reply rows; a long call must not retain every turn.
        while self
            .input
            .keys()
            .filter(|(session, _, _)| session == sid)
            .count()
            > MAX_LATENCY_REPLIES
        {
            let oldest = self
                .input
                .keys()
                .filter(|(session, _, _)| session == sid)
                .min_by_key(|(_, _, revision)| revision)
                .cloned();
            if let Some(oldest) = oldest {
                self.input.remove(&oldest);
            }
        }
        true
    }

    #[cfg(test)]
    pub(super) fn input_turns(&self) -> usize {
        self.input.len()
    }
    #[cfg(test)]
    pub(super) fn has_input(&self, sid: &str, thread: &str, revision: u64) -> bool {
        self.input
            .contains_key(&(sid.into(), thread.into(), revision))
    }
}

/// The stages an event ends: each with the event that started it, and whether that start must
/// belong to the same utterance.
fn stages_ended_by(event: LatencyEvent) -> &'static [(&'static str, LatencyEvent, bool)] {
    match event {
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
        // The reply handed to the call as text; the stage keeps the name it is reported under.
        LatencyEvent::ReplyDispatched => {
            &[("reply_to_synthesis", LatencyEvent::ReplyReceived, true)]
        }
        _ => &[],
    }
}

fn upsert(durations: &mut Vec<LatencyDuration>, duration: LatencyDuration) {
    if let Some(existing) = durations.iter_mut().find(|item| item.name == duration.name) {
        *existing = duration;
    } else {
        durations.push(duration);
    }
}
