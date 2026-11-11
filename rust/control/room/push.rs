//! Push delivery, in two phases: at least once, to a connector that never hands the agent the same message twice.
//!
//! 1. **Until `accepted`:** the room offers a conversation's first unresolved message with `input.deliver`, and offers it
//!    again on the back-off, without limit, until the connector says the agent took it (`accepted`). Later messages of
//!    the conversation wait behind it. Only a conversation that is gone lets go of it (`cancel_unsent`).
//! 2. **After `accepted`, until `received`:** the room never offers it again; it asks, for all such messages of a
//!    conversation at once (`input.received`), which ones the agent has. The connector answers from its own record of
//!    each message id without touching the agent, so a receipt lost on the way is recovered by the next question. An id
//!    the connector has no record of was never handed over, and goes back to phase 1.
//!
//! The connector also tells a receipt as soon as it sees it (`input.read`); the question is the safety net.
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::{json, Value};

use super::journal::InputRef;
use super::latency::{latency_now_micros, LatencyEvent};
use super::peers::{ConnectorPeer, PeerError};
use super::util::seconds;
use super::{Inner, Room};
use crate::control::telemetry::Counted;

const ACK_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the room waits for the connector to say which messages the agent has.
const RECEIVED_TIMEOUT: Duration = Duration::from_secs(10);
/// Most message ids asked about in one question.
const RECEIVED_BATCH: usize = 64;
/// Seconds before another attempt (an offer in phase 1, a question in phase 2), by attempts made so far: fast at first,
/// then steady (every 15 s, for as long as it takes). A pass every 15 s per waiting conversation is trivial for the room.
/// A message of a conversation waits for the earlier ones (`pending_delivery` takes a conversation's first unresolved row
/// only), so a retry never lets a later one pass.
const RETRY_SECONDS: [u64; 5] = [1, 2, 5, 10, 15];
/// Failed offers after which a note is given up on. Notes are exempt from expiry, so without this one a note nobody can
/// take would be offered forever.
const MAX_NOTE_ATTEMPTS: usize = 5;
/// The longest `detail` of a delivery answer the room keeps, in bytes.
const MAX_DETAIL: usize = 1000;

fn retry_after(attempts: usize) -> u64 {
    RETRY_SECONDS[attempts.clamp(1, RETRY_SECONDS.len()) - 1]
}

/// The status a connector answered a delivery with. Every other key of the answer is ignored; a status the
/// room does not know (or none) is tried again later, as `failed` is.
fn delivery_status(value: &Value) -> Option<&str> {
    value.get("status").and_then(Value::as_str)
}

/// The `detail` of a delivery answer, when it is text of at most [`MAX_DETAIL`] bytes; otherwise none.
fn delivery_detail(value: &Value) -> Option<String> {
    value
        .get("detail")
        .and_then(Value::as_str)
        .filter(|detail| detail.len() <= MAX_DETAIL)
        .map(str::to_owned)
}

/// The message ids listed under `key` of a connector's answer; anything else there is ignored.
fn listed<'a>(answer: &'a Value, key: &str) -> HashSet<&'a str> {
    answer
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// One question to a connector: which of these messages, handed to one conversation, its agent has.
pub struct ReceiptQuestion {
    pub binding_id: String,
    pub peer: ConnectorPeer,
    pub message_ids: Vec<String>,
}

impl Room {
    pub fn pending_delivery(&self) -> Vec<(String, String, ConnectorPeer, Value)> {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        inner.materialize_due_notes();
        let now = seconds();
        for input in inner.journal.expire_input(now) {
            inner.browsers.input_receipt(&input, "not_sent");
        }
        let mut work = Vec::new();
        let mut blocked = HashSet::new();
        for row in inner.journal.unresolved_input() {
            if !blocked.insert(row.thread.clone())
                || row.status != "pending"
                || row.next_attempt > now
            {
                continue;
            }
            let Some(binding) = inner
                .bindings
                .push_target(&row.thread, |bid| inner.inflight.is_busy(bid))
            else {
                continue;
            };
            let Some(peer) = inner.peers.get(&binding.connector).cloned() else {
                continue;
            };
            let payload = row.payload.as_ref().unwrap();
            let mut data = json!({"event_id":row.id,"binding_id":binding.id,"thread":row.thread,"text":row.text,
                "channel":payload["channel"],"session_id":payload["session_id"],"revision":payload["revision"],
                "epoch":payload["epoch"],"message_id":payload["message_id"]});
            if let Some(unheard) = payload.get("unheard") {
                data["unheard"] = unheard.clone();
            }
            if let Some(notice) = payload.get("notice") {
                data["notice"] = notice.clone();
            }
            row.ever_exposed = true;
            row.status = "sending".into();
            inner.inflight.start(&binding.id, &row.id);
            work.push((binding.id.clone(), row.id.clone(), peer, data));
        }
        work
    }
    pub fn settle_delivery(
        &self,
        bid: &str,
        rid: &str,
        generation: &str,
        answer: Result<Value, PeerError>,
    ) {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if !inner.inflight.awaits(bid, rid) {
            return;
        }
        let Some(b) = inner.bindings.get(bid) else {
            return;
        };
        if !inner.peers.is_current(&b.connector, generation) {
            return;
        }
        let harness = b.harness.clone();
        let thread = b.thread.clone();
        inner.inflight.finish(bid);
        let status = answer.as_ref().ok().and_then(delivery_status);
        let received = answer
            .as_ref()
            .is_ok_and(|answer| answer.get("received") == Some(&Value::Bool(true)));
        let Some(row) = inner.journal.find_mut(rid) else {
            return;
        };
        let note = row.role == "note";
        if row.status == "read" {
            return;
        }
        let new_status = match status {
            // In the agent's inbox (an older connector's `unknown` says the same: handed over, receipt pending). From
            // here on the room only asks whether the agent has it.
            Some("accepted" | "unknown") => "delivered",
            // The harness cannot put it in now (no route, or the chat left the session that took it), but the
            // conversation is still there: the message waits, offered again on the back-off and readable by
            // get_messages meanwhile. Only a conversation that is gone lets go of it (`cancel_unsent`).
            _ if note && row.attempts + 1 >= MAX_NOTE_ATTEMPTS => "not_sent",
            _ => "pending",
        };
        row.status = new_status.into();
        // Why the last offer was not taken, while it waits; nothing once it was.
        row.reason = match (new_status, status) {
            ("pending", Some("unsupported")) => Some("unsupported".into()),
            ("pending", Some(_)) => answer.as_ref().ok().and_then(delivery_detail),
            _ => None,
        };
        let first_retry = new_status == "pending" && row.attempts == 0;
        if new_status == "pending" {
            row.attempts += 1;
            row.next_attempt = seconds() + retry_after(row.attempts);
        } else {
            // Phase 2 starts its own count: the first question soon, then on the same back-off.
            row.attempts = 0;
            row.next_attempt = seconds() + retry_after(1);
        }
        let input = row.input_ref();
        // Counted once per message that needed another offer, not once per offer.
        if let (Some(telemetry), true) = (crate::control::telemetry::shared(), first_retry) {
            telemetry.count(
                Counted::Redeliveries,
                &json!({"sidevoice.thread_id": input.thread, "sidevoice.harness": harness}),
            );
        }
        if new_status == "delivered" {
            if let Some(c) = inner.browsers.get_mut(&input.session) {
                c.sent += 1;
            }
        }
        inner.browsers.input_receipt(&input, new_status);
        if new_status == "not_sent" {
            inner.salvage_input_notice(&input.id);
        }
        if new_status == "delivered" {
            inner.mark_latency(
                &input.session,
                &thread,
                input.revision,
                None,
                LatencyEvent::DeliveryAccepted,
                latency_now_micros(),
            );
            if received {
                inner.settle_received(&input);
            }
        }
        // A failed write is tried again by the pump; the row is at worst offered once more after a restart.
        let _ = inner.persist_pending();
    }

    /// The questions due now: per conversation with a live binding, the messages its agent took and has not been
    /// seen to have, oldest first. Each is not asked about again until the answer is settled or its wait runs out.
    pub fn pending_receipts(&self) -> Vec<ReceiptQuestion> {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let now = seconds();
        let mut questions: HashMap<String, ReceiptQuestion> = HashMap::new();
        for row in inner.journal.awaiting_receipt() {
            if row.next_attempt > now {
                continue;
            }
            let Some(binding) = inner.bindings.delivery_target(&row.thread) else {
                continue;
            };
            let Some(peer) = inner.peers.get(&binding.connector) else {
                continue;
            };
            let question = questions
                .entry(binding.id.clone())
                .or_insert_with(|| ReceiptQuestion {
                    binding_id: binding.id.clone(),
                    peer: peer.clone(),
                    message_ids: Vec::new(),
                });
            if question.message_ids.len() >= RECEIVED_BATCH {
                continue;
            }
            question.message_ids.push(row.message_id().to_owned());
            // Not asked again while this question waits; settling sets the next one.
            row.next_attempt = now + RECEIVED_TIMEOUT.as_secs() + retry_after(row.attempts + 1);
        }
        questions.into_values().collect()
    }

    /// The connector's answer to a question about `message_ids` of binding `bid`'s conversation: those `received` are
    /// read; those it has no record of (`unknown`) are offered again; the rest are asked about again on the back-off.
    pub fn settle_receipts(
        &self,
        bid: &str,
        generation: &str,
        message_ids: &[String],
        answer: Result<Value, PeerError>,
    ) {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let Some(binding) = inner.bindings.get(bid) else {
            return;
        };
        if !inner.peers.is_current(&binding.connector, generation) {
            return;
        }
        let thread = binding.thread.clone();
        let (received, unknown) = match &answer {
            Ok(answer) => (listed(answer, "received"), listed(answer, "unknown")),
            Err(_) => Default::default(),
        };
        let now = seconds();
        let mut read = Vec::new();
        let mut again = Vec::new();
        for row in inner
            .journal
            .input_mut(&thread)
            .filter(|row| row.status == "delivered" && row.pull_claimed_by.is_none())
        {
            let id = row.message_id().to_owned();
            if !message_ids.contains(&id) {
                continue;
            }
            if received.contains(id.as_str()) {
                read.push(row.input_ref());
            } else if unknown.contains(id.as_str()) {
                row.requeue();
                row.attempts = 0;
                again.push(row.input_ref());
            } else {
                row.attempts += 1;
                row.next_attempt = now + retry_after(row.attempts);
            }
        }
        for input in &read {
            inner.settle_received(input);
        }
        for input in &again {
            inner.browsers.input_receipt(input, "pending");
        }
        let _ = inner.persist_pending();
    }

    pub async fn pump(self: std::sync::Arc<Self>) {
        loop {
            {
                let room = self.clone();
                let _ = tokio::task::spawn_blocking(move || room.persist_pending()).await;
            }
            for (bid, rid, peer, data) in self.pending_delivery() {
                let room = self.clone();
                tokio::spawn(async move {
                    let answer = peer.request("input.deliver", data, ACK_TIMEOUT).await;
                    room.settle_delivery(&bid, &rid, &peer.generation, answer);
                });
            }
            for question in self.pending_receipts() {
                let room = self.clone();
                tokio::spawn(async move {
                    let ReceiptQuestion {
                        binding_id,
                        peer,
                        message_ids,
                    } = question;
                    let answer = peer
                        .request(
                            "input.received",
                            json!({"binding_id":binding_id,"message_ids":message_ids}),
                            RECEIVED_TIMEOUT,
                        )
                        .await;
                    room.settle_receipts(&binding_id, &peer.generation, &message_ids, answer);
                });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

impl Inner {
    /// The agent has `input`: it is read, and the call that sent it is told.
    pub(super) fn settle_received(&mut self, input: &InputRef) {
        let Some(row) = self.journal.find_mut(&input.id) else {
            return;
        };
        if matches!(row.status.as_str(), "read" | "not_sent") {
            return;
        }
        row.status = "read".into();
        row.reason = None;
        row.pull_claimed_by = None;
        self.browsers.input_receipt(input, "read");
        if let Some(thread) = input.thread.as_deref() {
            self.mark_latency(
                &input.session,
                thread,
                input.revision,
                None,
                LatencyEvent::Read,
                latency_now_micros(),
            );
        }
    }
}
