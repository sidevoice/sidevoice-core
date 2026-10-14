//! Push delivery: handing pending input to a connector and settling its acknowledgement.
use std::time::Duration;

use serde_json::{json, Value};

use super::latency::{latency_now_micros, LatencyEvent};
use super::peers::{ConnectorPeer, PeerError};
use super::util::seconds;
use super::Room;
use crate::control::telemetry::Counted;

const ACK_TIMEOUT: Duration = Duration::from_secs(60);
/// Seconds before another attempt, by attempts made so far.
const RETRY_SECONDS: [u64; 4] = [2, 5, 15, 60];
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
        for row in inner.journal.due_input(now) {
            // A pull binding consumes the room journal directly. Falling back to an older
            // push binding here would deliver the same words twice.
            if inner
                .bindings
                .delivery_target(&row.thread)
                .is_some_and(|b| b.pull_input)
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
        inner.inflight.finish(bid);
        let status = answer.as_ref().ok().and_then(delivery_status);
        let Some(row) = inner.journal.find_mut(rid) else {
            return;
        };
        let note = row.role == "note";
        if row.status == "read" {
            return;
        }
        let new_status = match status {
            Some("accepted") => "delivered",
            Some("unknown") if !note => "unconfirmed",
            Some("unsupported") if !note => "not_sent",
            _ => "pending",
        };
        row.status = new_status.into();
        row.reason = match status {
            Some("unknown") => answer.as_ref().ok().and_then(delivery_detail),
            Some("unsupported") => Some("unsupported".into()),
            _ => None,
        };
        if new_status == "pending" {
            row.attempts += 1;
            row.next_attempt = seconds() + retry_after(row.attempts);
        }
        let input = row.input_ref();
        if let (Some(telemetry), "pending") = (crate::control::telemetry::shared(), new_status) {
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
        if let (Some(thread), "delivered") = (input.thread.as_deref(), new_status) {
            inner.mark_latency(
                &input.session,
                thread,
                input.revision,
                None,
                LatencyEvent::DeliveryAccepted,
                latency_now_micros(),
            );
        }
    }
    pub async fn pump(self: std::sync::Arc<Self>) {
        loop {
            for (bid, rid, peer, data) in self.pending_delivery() {
                let room = self.clone();
                tokio::spawn(async move {
                    let answer = peer.request("input.deliver", data, ACK_TIMEOUT).await;
                    room.settle_delivery(&bid, &rid, &peer.generation, answer);
                });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}
