//! The latency view's rules:
//! intervals only within the matching turn, first observation wins, missing is not zero, rows are
//! bounded, and every client- or provider-supplied duration is allowlisted and range-checked.

use serde_json::{json, Value};
use sidevoice_core::control::latency::{snapshot, Duration, Event, Mark, Reply};

fn mark(event: Event, utterance: Option<&'static str>, seconds: f64) -> Mark<'static> {
    Mark {
        session_id: "session",
        thread_id: "a",
        revision: 1,
        utterance_id: utterance,
        event,
        at_micros: (seconds * 1_000_000.0) as u64,
    }
}

fn reply<'a>(
    input_ms: &'a [Duration<'a>],
    provider_ms: &'a [Duration<'a>],
    browser_ms: &'a [Duration<'a>],
) -> Reply<'a> {
    Reply {
        session_id: "session",
        thread_id: "a",
        revision: 1,
        utterance_id: "u",
        status: "queued",
        synthesis_attempt: 1,
        input_ms,
        provider_ms,
        browser_ms,
    }
}

fn row(marks: &[Mark<'_>], reply: Reply<'_>) -> Value {
    snapshot("session", "en", marks, &[reply])["replies"][0].clone()
}

#[test]
fn server_intervals_include_only_the_matching_original_turn() {
    let mut marks = vec![
        mark(Event::Queued, None, 10.0),
        mark(Event::DeliveryAccepted, None, 10.2),
        mark(Event::ReplyReceived, Some("u"), 12.0),
        mark(Event::SynthesisStarted, Some("u"), 13.0),
        mark(Event::AudioReady, Some("u"), 14.0),
    ];
    // Another turn's and another thread's marks are not this reply's.
    marks.push(Mark {
        revision: 2,
        ..mark(Event::Read, None, 11.0)
    });
    marks.push(Mark {
        thread_id: "b",
        ..mark(Event::Read, None, 11.0)
    });
    assert_eq!(
        row(&marks, reply(&[], &[], &[]))["server_ms"],
        json!({
            "input_queued_to_delivery_accepted_ms": 200.0,
            "input_queued_to_reply_received_ms": 2000.0,
            "delivery_accepted_to_reply_received_ms": 1800.0,
            "reply_received_to_synthesis_started_ms": 1000.0,
            "synthesis_started_to_audio_ready_ms": 1000.0,
        })
    );
}

#[test]
fn duplicate_events_keep_the_first_time_and_missing_is_not_zero() {
    let marks = [
        mark(Event::Queued, None, 10.0),
        mark(Event::Queued, None, 12.0),
        mark(Event::ReplyReceived, Some("u"), 12.0),
        mark(Event::ReplyReceived, Some("u"), 15.0),
    ];
    assert_eq!(
        row(&marks, reply(&[], &[], &[]))["server_ms"],
        json!({"input_queued_to_reply_received_ms": 2000.0})
    );
}

#[test]
fn replies_are_bounded_and_namespaced() {
    let ids: Vec<String> = (0..130).map(|n| format!("u{n}")).collect();
    let replies: Vec<_> = ids
        .iter()
        .map(|id| Reply {
            thread_id: "b",
            utterance_id: id,
            ..reply(&[], &[], &[])
        })
        .collect();
    let marks = [
        mark(Event::Queued, None, 10.0),
        mark(Event::ReplyReceived, Some("u0"), 12.0),
    ];
    let view = snapshot("session", "en", &marks, &replies);
    assert_eq!(view["limit"], 128);
    assert_eq!(view["replies"].as_array().unwrap().len(), 128);
    assert_eq!(
        view["replies"][0]["server_ms"],
        json!({}),
        "thread a's turn is not thread b's"
    );
    assert_eq!(
        snapshot("other", "en", &marks, &replies)["replies"],
        json!([])
    );
}

#[test]
fn client_durations_are_untrusted_bounded_and_separate() {
    let browser = [
        Duration {
            name: "audio_received_to_playback_scheduled_ms",
            milliseconds: 125.25,
        },
        Duration {
            name: "turn_finished_event_to_playback_scheduled_ms",
            milliseconds: f64::NAN,
        },
        Duration {
            name: "turn_finished_event_to_playback_scheduled_ms",
            milliseconds: 3_600_001.0,
        },
        Duration {
            name: "turn_finished_event_to_playback_scheduled_ms",
            milliseconds: f64::INFINITY,
        },
        Duration {
            name: "vad_stop_event_to_turn_finished_event_ms",
            milliseconds: -1.0,
        },
        Duration {
            name: "text",
            milliseconds: 1.0,
        },
    ];
    let view = row(&[], reply(&[], &[], &browser));
    assert_eq!(
        view["browser_ms"],
        json!({"audio_received_to_playback_scheduled_ms": 125.25})
    );
    assert_eq!(view["server_ms"], json!({}));
}

#[test]
fn input_and_provider_durations_are_allowlisted() {
    let input = [
        Duration {
            name: "audio_ms",
            milliseconds: 8000.0,
        },
        Duration {
            name: "endpoint_silence_ms",
            milliseconds: 2500.0,
        },
        Duration {
            name: "recognition_ms",
            milliseconds: 900.0,
        },
        Duration {
            name: "speech_end_to_transcript_ms",
            milliseconds: 3425.0,
        },
        Duration {
            name: "transcript",
            milliseconds: 1.0,
        },
        Duration {
            name: "audio_ms",
            milliseconds: -1.0,
        },
    ];
    let provider = [
        Duration {
            name: "request_to_first_chunk_ms",
            milliseconds: 10.0,
        },
        Duration {
            name: "audio_base64",
            milliseconds: 1.0,
        },
    ];
    let view = row(&[], reply(&input, &provider, &[]));
    assert_eq!(
        view["input_ms"],
        json!({"audio_ms": 8000.0, "endpoint_silence_ms": 2500.0,
            "recognition_ms": 900.0, "speech_end_to_transcript_ms": 3425.0})
    );
    assert_eq!(
        view["provider_ms"],
        json!({"request_to_first_chunk_ms": 10.0})
    );
}
