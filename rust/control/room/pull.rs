//! Pull delivery: a pull binding reading the journal directly, with claims held until acknowledged.
use serde_json::{json, Value};

use super::bindings::Binding;
use super::error::RoomError;
use super::journal::{input_receipt, Row, INPUT_TTL};
use super::latency::{latency_now_micros, mark_latency, LatencyEvent};
use super::util::{field, seconds};
use super::{Inner, Room};

/// Most pull messages one fetch returns, and most IDs one fetch may acknowledge.
pub(super) const PULL_PAGE: usize = 32;

impl Room {
    /// Read the existing in-memory journal for one live pull binding. A fetched message is
    /// claimed by that binding and returned again on every later fetch until the caller
    /// acknowledges its ID; the claim is released if the binding stops being the thread's live
    /// pull binding. Acknowledgement is idempotent: an ID this binding does not hold is ignored.
    pub fn pull_input(&self, cid: &str, data: &Value) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let bid = field(data, "binding_id");
        let Some(binding) = inner.bindings.get(bid).filter(|binding| {
            binding.connector == cid && binding.active && binding.live && binding.pull_input
        }) else {
            return Err(RoomError::new(403, "room.pull_binding_invalid"));
        };
        let thread = binding.thread.clone();
        // Only the binding that would receive the thread's input may read it; an older or
        // newer binding on the same thread would otherwise see words also delivered elsewhere.
        if delivery_binding(&inner, &thread).is_none_or(|b| b.id != bid) {
            return Err(RoomError::new(409, "room.pull_binding_superseded"));
        }
        let operation = field(data, "operation");
        if !matches!(operation, "check" | "get") {
            return Err(RoomError::new(400, "room.pull_operation_invalid"));
        }
        let ack_ids = data.get("ack_ids");
        if operation == "check" && ack_ids.is_some() {
            return Err(RoomError::new(400, "room.pull_ack_invalid"));
        }
        let ack_ids: Vec<&str> = match ack_ids {
            None => Vec::new(),
            Some(Value::Array(ids)) if ids.len() <= PULL_PAGE => ids
                .iter()
                .map(|id| id.as_str().filter(|id| !id.is_empty()))
                .collect::<Option<Vec<&str>>>()
                .ok_or(RoomError::new(400, "room.pull_ack_invalid"))?,
            Some(_) => return Err(RoomError::new(400, "room.pull_ack_invalid")),
        };
        let after = match data.get("after") {
            None => 0,
            Some(value) => value
                .as_u64()
                .ok_or(RoomError::new(400, "room.pull_cursor_invalid"))?,
        };
        let held = |row: &Row| {
            row.thread == thread
                && row.role == "user"
                && row.status == "delivered"
                && row.pull_claimed_by.as_deref() == Some(bid)
        };
        let mut read = Vec::new();
        for row in inner.rows.iter_mut().filter(|row| {
            held(row)
                && row
                    .payload
                    .as_ref()
                    .is_some_and(|payload| ack_ids.contains(&field(payload, "message_id")))
        }) {
            row.status = "read".into();
            read.push((row.session.clone(), row.id.clone(), row.revision));
        }
        let acknowledged = read.len();
        for (session, history_id, revision) in read {
            input_receipt(&inner, &session, &history_id, &thread, revision, "read");
            mark_latency(
                &mut inner,
                &session,
                &thread,
                revision,
                None,
                LatencyEvent::Read,
                latency_now_micros(),
            );
        }
        // Undelivered input expires as it does for push; a claimed row stays until it is
        // acknowledged, released or trimmed from the bounded journal.
        let now = seconds();
        let mut count = 0usize;
        let mut messages = Vec::new();
        let mut claimed = Vec::new();
        let mut more = false;
        let mut fresh = 0usize;
        for row in inner.rows.iter_mut().filter(|row| {
            held(row)
                || (row.thread == thread
                    && row.role == "user"
                    && row.status == "pending"
                    && now.saturating_sub(row.queued_at) < INPUT_TTL)
        }) {
            count += 1;
            if operation == "get" && row.seq > after {
                if messages.len() >= PULL_PAGE {
                    more = true;
                } else {
                    let payload = row.payload.clone().unwrap_or_default();
                    messages.push(json!({"message_id":payload["message_id"],
                        "session_id":payload["session_id"], "revision":payload["revision"],
                        "channel":"voice", "text":row.text, "arrival_time":row.time,
                        "cursor":row.seq}));
                    if row.status == "pending" {
                        row.status = "delivered".into();
                        row.pull_claimed_by = Some(bid.to_owned());
                        claimed.push((row.session.clone(), row.id.clone(), row.revision));
                    }
                }
            }
            // Never fetched by anyone: what a hook check uses to ask for one retrieval
            // without asking again for messages the agent already holds.
            if row.status == "pending" {
                fresh += 1;
            }
        }
        for (session, history_id, revision) in claimed {
            if let Some(browser) = inner.browsers.get_mut(&session) {
                browser.sent += 1;
            }
            input_receipt(
                &inner,
                &session,
                &history_id,
                &thread,
                revision,
                "delivered",
            );
            mark_latency(
                &mut inner,
                &session,
                &thread,
                revision,
                None,
                LatencyEvent::DeliveryAccepted,
                latency_now_micros(),
            );
        }
        let cursor = messages
            .last()
            .and_then(|message| message["cursor"].as_u64())
            .unwrap_or(after);
        Ok(json!({"connected":true, "pending":count > 0, "count":count,
            "fresh":fresh, "messages":messages, "cursor":cursor, "more":more,
            "acknowledged":acknowledged}))
    }
}
/// The live binding a thread's input goes to first: the newest, ties broken by ID so push and
/// pull agree on it.
pub(super) fn delivery_binding<'a>(inner: &'a Inner, thread: &str) -> Option<&'a Binding> {
    inner
        .bindings
        .values()
        .filter(|b| b.active && b.live && b.thread == thread)
        .max_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)))
}
/// Return fetched but unacknowledged pull input to the queue, so that the thread's next
/// delivery binding (pull or push) receives it rather than leaving it claimed by a binding
/// that can no longer read it. The same message ID may therefore be fetched again.
pub(super) fn release_pull_claims(inner: &mut Inner, released: impl Fn(&Row) -> bool) {
    let mut back = Vec::new();
    for row in inner
        .rows
        .iter_mut()
        .filter(|row| row.status == "delivered" && row.pull_claimed_by.is_some())
        .filter(|row| released(row))
    {
        row.status = "pending".into();
        row.pull_claimed_by = None;
        row.next_attempt = 0;
        back.push((
            row.session.clone(),
            row.id.clone(),
            row.thread.clone(),
            row.revision,
        ));
    }
    for (session, history_id, thread, revision) in back {
        input_receipt(inner, &session, &history_id, &thread, revision, "pending");
    }
}
