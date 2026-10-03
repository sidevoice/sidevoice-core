//! Read-only formatting of Room and call-owned, monotonic latency observations.
//! The caller owns mark storage, session admission, revisions and media lifecycle.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::messages::{render, LocalizedMessage};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Event {
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

/// A timestamp from the same process monotonic clock, in microseconds. T3 owns
/// delivery/read/reply marks; T5 supplies call observations through Room.
#[derive(Clone, Copy, Debug)]
pub struct Mark<'a> {
    pub session_id: &'a str,
    pub thread_id: &'a str,
    pub revision: u64,
    pub utterance_id: Option<&'a str>,
    pub event: Event,
    pub at_micros: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Duration<'a> {
    pub name: &'a str,
    pub milliseconds: f64,
}

/// Current Room-owned reply view. The slices refer to immutable T3/T5 values;
/// this module retains none of them after formatting a response.
pub struct Reply<'a> {
    pub session_id: &'a str,
    pub thread_id: &'a str,
    pub revision: u64,
    pub utterance_id: &'a str,
    pub status: &'a str,
    pub synthesis_attempt: u32,
    pub input_ms: &'a [Duration<'a>],
    pub provider_ms: &'a [Duration<'a>],
    pub browser_ms: &'a [Duration<'a>],
}

fn durations(values: &[Duration<'_>], allowed: &[&str]) -> Value {
    let mut object = Map::new();
    for item in values {
        if allowed.contains(&item.name)
            && item.milliseconds.is_finite()
            && (0.0..=3_600_000.0).contains(&item.milliseconds)
        {
            object.insert(item.name.to_owned(), json!((item.milliseconds * 100.0).round() / 100.0));
        }
    }
    Value::Object(object)
}

fn interval(output: &mut Map<String, Value>, name: &str, start: Option<u64>, end: Option<u64>) {
    if let (Some(start), Some(end)) = (start, end) {
        if end >= start {
            output.insert(name.to_owned(), json!(((end - start) as f64 / 10.0).round() / 100.0));
        }
    }
}

/// Preserve the Python `/api/presentation/latency` response shape. Only the
/// authenticated session's live Room view may be passed to this formatter.
pub fn snapshot(session_id: &str, marks: &[Mark<'_>], replies: &[Reply<'_>]) -> Value {
    type Key<'a> = (&'a str, u64, Option<&'a str>);
    let mut indexed: HashMap<Key<'_>, HashMap<Event, u64>> = HashMap::new();
    for mark in marks.iter().filter(|mark| mark.session_id == session_id) {
        indexed
            .entry((mark.thread_id, mark.revision, mark.utterance_id))
            .or_default()
            .entry(mark.event)
            .or_insert(mark.at_micros);
    }
    let mut rows = Vec::new();
    for reply in replies.iter().filter(|reply| reply.session_id == session_id).take(128) {
        let turn = indexed.get(&(reply.thread_id, reply.revision, None));
        let spoken = indexed.get(&(reply.thread_id, reply.revision, Some(reply.utterance_id)));
        let t = |event| turn.and_then(|m| m.get(&event)).copied();
        let r = |event| spoken.and_then(|m| m.get(&event)).copied();
        let mut server_ms = Map::new();
        for (name, start, end) in [
            ("input_queued_to_delivery_accepted_ms", t(Event::Queued), t(Event::DeliveryAccepted)),
            ("input_queued_to_reply_received_ms", t(Event::Queued), r(Event::ReplyReceived)),
            ("delivery_accepted_to_reply_received_ms", t(Event::DeliveryAccepted), r(Event::ReplyReceived)),
            ("delivery_accepted_to_read_ms", t(Event::DeliveryAccepted), t(Event::Read)),
            ("input_queued_to_read_ms", t(Event::Queued), t(Event::Read)),
            ("read_to_reply_received_ms", t(Event::Read), r(Event::ReplyReceived)),
            ("reply_received_to_synthesis_started_ms", r(Event::ReplyReceived), r(Event::SynthesisStarted)),
            ("synthesis_started_to_audio_ready_ms", r(Event::SynthesisStarted), r(Event::AudioReady)),
            ("audio_dispatched_to_playing_receipt_ms", r(Event::AudioDispatched), r(Event::PlayingReceipt)),
        ] {
            interval(&mut server_ms, name, start, end);
        }
        rows.push(json!({
            "utterance_id":reply.utterance_id,"thread_id":reply.thread_id,
            "reply_revision":reply.revision,"status":reply.status,
            "synthesis_attempt":reply.synthesis_attempt,
            "input_ms":durations(reply.input_ms,&["audio_ms","endpoint_silence_ms","recognition_ms","request_to_transcript_ms","speech_end_to_transcript_ms","transcript_to_delivery_ms"]),
            "server_ms":server_ms,
            "provider_ms":durations(reply.provider_ms,&["request_to_headers_ms","request_to_first_chunk_ms","request_to_complete_ms"]),
            "browser_ms":durations(reply.browser_ms,&["audio_received_to_playback_scheduled_ms","turn_finished_event_to_playback_scheduled_ms"]),
        }));
    }
    let notes = ["latency.note_clocks", "latency.note_delivery", "latency.note_playback",
        "latency.note_input", "latency.note_missing"]
        .map(|key| render(&LocalizedMessage::new(key), "en"));
    json!({"session_id":session_id,"limit":128,"replies":rows,"notes":notes})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_other_sessions_and_preserves_python_shape() {
        let marks = [
            Mark {session_id:"own",thread_id:"thread",revision:2,utterance_id:None,event:Event::Queued,at_micros:1_000_000},
            Mark {session_id:"own",thread_id:"thread",revision:2,utterance_id:None,event:Event::Read,at_micros:1_250_000},
            Mark {session_id:"own",thread_id:"thread",revision:2,utterance_id:Some("reply"),event:Event::ReplyReceived,at_micros:1_500_000},
            Mark {session_id:"other",thread_id:"thread",revision:2,utterance_id:Some("reply"),event:Event::SynthesisStarted,at_micros:1_550_000},
        ];
        let replies = [Reply {session_id:"own",thread_id:"thread",revision:2,utterance_id:"reply",status:"received",synthesis_attempt:0,
            input_ms:&[Duration{name:"audio_ms",milliseconds:123.456},Duration{name:"transcript",milliseconds:999.0}],
            provider_ms:&[],browser_ms:&[]},
            Reply {session_id:"other",thread_id:"thread",revision:2,utterance_id:"secret",status:"received",synthesis_attempt:0,
            input_ms:&[],provider_ms:&[],browser_ms:&[]}];
        let result = snapshot("own", &marks, &replies);
        assert_eq!(result["replies"].as_array().unwrap().len(),1);
        assert_eq!(result["replies"][0]["input_ms"],json!({"audio_ms":123.46}));
        assert_eq!(result["replies"][0]["server_ms"]["input_queued_to_reply_received_ms"],500.0);
        assert_eq!(result["replies"][0]["server_ms"]["read_to_reply_received_ms"],250.0);
        assert!(result["replies"][0]["server_ms"].get("reply_received_to_synthesis_started_ms").is_none());
        assert!(!result.to_string().contains("secret"));
    }
}
