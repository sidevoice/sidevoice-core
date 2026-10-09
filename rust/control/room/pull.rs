//! Pull delivery: a pull binding reading the journal directly, with claims held until acknowledged.
use serde_json::{json, Value};

use super::error::RoomError;
use super::journal::Row;
use super::latency::{latency_now_micros, LatencyEvent};
use super::util::{field, seconds};
use super::{Inner, Room};

/// Most pull messages one fetch returns, and most IDs one fetch may acknowledge.
pub(super) const PULL_PAGE: usize = 32;

/// A validated pull request.
struct PullRequest<'a> {
    /// Fetch messages; a check only counts them.
    get: bool,
    ack_ids: Vec<&'a str>,
    after: u64,
}
impl<'a> PullRequest<'a> {
    fn parse(data: &'a Value) -> Result<Self, RoomError> {
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
        Ok(Self {
            get: operation == "get",
            ack_ids,
            after,
        })
    }
}

/// What one fetch saw of a thread's input.
#[derive(Default)]
struct Page {
    count: usize,
    fresh: usize,
    messages: Vec<Value>,
    more: bool,
}

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
        if inner
            .bindings
            .delivery_target(&thread)
            .is_none_or(|b| b.id != bid)
        {
            return Err(RoomError::new(409, "room.pull_binding_superseded"));
        }
        let request = PullRequest::parse(data)?;
        let acknowledged = inner.acknowledge_pull(bid, &thread, &request.ack_ids);
        let page = inner.fetch_pull(bid, &thread, &request);
        let cursor = page
            .messages
            .last()
            .and_then(|message| message["cursor"].as_u64())
            .unwrap_or(request.after);
        Ok(
            json!({"connected":true, "pending":page.count > 0, "count":page.count,
            "fresh":page.fresh, "messages":page.messages, "cursor":cursor, "more":page.more,
            "acknowledged":acknowledged}),
        )
    }
}

/// Input binding `bid` fetched and has not acknowledged yet.
fn held(row: &Row, bid: &str) -> bool {
    row.status == "delivered" && row.pull_claimed_by.as_deref() == Some(bid)
}

impl Inner {
    /// Mark read the held input whose message IDs were acknowledged; returns how many.
    fn acknowledge_pull(&mut self, bid: &str, thread: &str, ack_ids: &[&str]) -> usize {
        let read: Vec<_> = self
            .journal
            .input_mut(thread)
            .filter(|row| held(row, bid) && ack_ids.contains(&row.message_id()))
            .map(|row| {
                row.status = "read".into();
                row.input_ref()
            })
            .collect();
        for input in &read {
            self.browsers.input_receipt(input, "read");
            self.mark_latency(
                &input.session,
                thread,
                input.revision,
                None,
                LatencyEvent::Read,
                latency_now_micros(),
            );
        }
        read.len()
    }
    /// Count the input binding `bid` holds or may still fetch, and on a get, return the next
    /// page after the cursor, claiming what it returns for the first time.
    fn fetch_pull(&mut self, bid: &str, thread: &str, request: &PullRequest<'_>) -> Page {
        // Undelivered input expires as it does for push; a claimed row stays until it is
        // acknowledged, released or trimmed from the bounded journal.
        let now = seconds();
        let mut page = Page::default();
        let mut claimed = Vec::new();
        for row in self
            .journal
            .input_mut(thread)
            .filter(|row| held(row, bid) || (row.status == "pending" && !row.expired(now)))
        {
            page.count += 1;
            if request.get && row.seq > request.after {
                if page.messages.len() >= PULL_PAGE {
                    page.more = true;
                } else {
                    let payload = row.payload.clone().unwrap_or_default();
                    let mut message = json!({"message_id":payload["message_id"],
                        "session_id":payload["session_id"], "revision":payload["revision"],
                        "channel":"voice", "text":row.text, "arrival_time":row.time,
                        "cursor":row.seq});
                    if let Some(unheard) = payload.get("unheard") {
                        message["unheard"] = unheard.clone();
                    }
                    page.messages.push(message);
                    if row.status == "pending" {
                        row.status = "delivered".into();
                        row.pull_claimed_by = Some(bid.to_owned());
                        claimed.push(row.input_ref());
                    }
                }
            }
            // Never fetched by anyone: what a hook check uses to ask for one retrieval
            // without asking again for messages the agent already holds.
            if row.status == "pending" {
                page.fresh += 1;
            }
        }
        for input in claimed {
            if let Some(browser) = self.browsers.get_mut(&input.session) {
                browser.sent += 1;
            }
            self.browsers.input_receipt(&input, "delivered");
            self.mark_latency(
                &input.session,
                thread,
                input.revision,
                None,
                LatencyEvent::DeliveryAccepted,
                latency_now_micros(),
            );
        }
        page
    }
    /// Return fetched but unacknowledged pull input to the queue, so that the thread's next
    /// delivery binding (pull or push) receives it rather than leaving it claimed by a binding
    /// that can no longer read it. The same message ID may therefore be fetched again.
    pub(super) fn release_pull_claims(&mut self, released: impl Fn(&Row) -> bool) {
        for input in self.journal.release_claims(released) {
            self.browsers.input_receipt(&input, "pending");
        }
    }
}
