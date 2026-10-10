//! The session's latency snapshot, borrowing the room's records for formatting.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::Json;

use crate::control::latency;
use crate::control::room::{LatencyDuration, LatencyEvent, LatencyMark, LatencyReply};
use crate::server::refusal::{refuse, require_origin, Handled};
use crate::server::request::query;
use crate::server::{AppState, AuthenticatedDevice};

pub(super) async fn session_latency(
    State(state): State<Arc<AppState>>,
    Extension(AuthenticatedDevice(device)): Extension<AuthenticatedDevice>,
    uri: Uri,
    headers: HeaderMap,
) -> Handled {
    require_origin(&headers)?;
    let absent = || refuse("room.browser_absent", StatusCode::NOT_FOUND, &headers);
    let sid = query(&uri, "session_id").ok_or_else(absent)?;
    let (language, marks, replies) = state
        .room
        .latency_records(&sid, &device)
        .ok_or_else(absent)?;
    let input: Vec<_> = replies
        .iter()
        .map(|reply| durations(&reply.input_ms))
        .collect();
    let provider: Vec<_> = replies
        .iter()
        .map(|reply| durations(&reply.provider_ms))
        .collect();
    let browser: Vec<_> = replies
        .iter()
        .map(|reply| durations(&reply.browser_ms))
        .collect();
    let borrowed: Vec<_> = replies
        .iter()
        .enumerate()
        .map(|(index, reply)| {
            borrowed_reply(reply, &input[index], &provider[index], &browser[index])
        })
        .collect();
    let borrowed_marks: Vec<_> = marks.iter().map(borrowed_mark).collect();
    Ok(Json(latency::snapshot(
        &sid,
        &language,
        &borrowed_marks,
        &borrowed,
    ))
    .into_response())
}

fn durations(items: &[LatencyDuration]) -> Vec<latency::Duration<'_>> {
    items
        .iter()
        .map(|item| latency::Duration {
            name: &item.name,
            milliseconds: item.milliseconds,
        })
        .collect()
}

fn borrowed_reply<'a>(
    reply: &'a LatencyReply,
    input_ms: &'a [latency::Duration<'a>],
    provider_ms: &'a [latency::Duration<'a>],
    browser_ms: &'a [latency::Duration<'a>],
) -> latency::Reply<'a> {
    latency::Reply {
        session_id: &reply.session_id,
        thread_id: &reply.thread_id,
        revision: reply.revision,
        utterance_id: &reply.utterance_id,
        status: &reply.status,
        input_ms,
        provider_ms,
        browser_ms,
    }
}

fn borrowed_mark(mark: &LatencyMark) -> latency::Mark<'_> {
    latency::Mark {
        session_id: &mark.session_id,
        thread_id: &mark.thread_id,
        revision: mark.revision,
        utterance_id: mark.utterance_id.as_deref(),
        event: event(&mark.event),
        at_micros: mark.at_micros,
    }
}

fn event(event: &LatencyEvent) -> latency::Event {
    match event {
        LatencyEvent::SpeechEnd => latency::Event::SpeechEnd,
        LatencyEvent::TurnClosed => latency::Event::TurnClosed,
        LatencyEvent::Transcript => latency::Event::Transcript,
        LatencyEvent::TranscriptDelivered => latency::Event::TranscriptDelivered,
        LatencyEvent::Queued => latency::Event::Queued,
        LatencyEvent::DeliveryAccepted => latency::Event::DeliveryAccepted,
        LatencyEvent::Read => latency::Event::Read,
        LatencyEvent::ReplyReceived => latency::Event::ReplyReceived,
        LatencyEvent::ReplyDispatched => latency::Event::ReplyDispatched,
        LatencyEvent::AudioReady => latency::Event::AudioReady,
        LatencyEvent::AudioDispatched => latency::Event::AudioDispatched,
        LatencyEvent::PlayingReceipt => latency::Event::PlayingReceipt,
    }
}
