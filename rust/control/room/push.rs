//! Push delivery: handing pending input to a connector and settling its acknowledgement.
use std::time::Duration;

use serde_json::{json, Value};

use super::latency::{latency_now_micros, LatencyEvent};
use super::peers::{ConnectorPeer, PeerError};
use super::util::{field, seconds};
use super::Room;

const ACK_TIMEOUT: Duration = Duration::from_secs(60);

fn valid_delivery_ack(value: &Value) -> Option<&str> {
    let fields = value.as_object()?;
    if serde_json::to_vec(value).ok()?.len() > 4096
        || fields
            .keys()
            .any(|k| !["status", "detail", "error"].contains(&k.as_str()))
    {
        return None;
    }
    for key in ["detail", "error"] {
        if fields
            .get(key)
            .is_some_and(|v| v.as_str().is_none_or(|s| s.len() > 1000))
        {
            return None;
        }
    }
    let status = field(value, "status");
    [
        "accepted",
        "unknown",
        "unsupported",
        "failed",
        "unknown_binding",
    ]
    .contains(&status)
    .then_some(status)
}

impl Room {
    pub fn pending_delivery(&self) -> Vec<(String, String, ConnectorPeer, Value)> {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
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
            let data = json!({"event_id":row.id,"binding_id":binding.id,"thread":row.thread,"text":row.text,"channel":"voice","session_id":payload["session_id"],"revision":payload["revision"],"message_id":payload["message_id"]});
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
        inner.inflight.finish(bid);
        let Some(row) = inner.journal.find_mut(rid) else {
            return;
        };
        if row.status == "read" {
            return;
        }
        let status = answer.as_ref().ok().and_then(valid_delivery_ack);
        let new_status = match status {
            Some("accepted") => "delivered",
            Some("unknown") => "unconfirmed",
            Some("unsupported") => "not_sent",
            _ => "pending",
        };
        row.status = new_status.into();
        row.reason = match status {
            Some("unknown") => answer
                .as_ref()
                .ok()
                .and_then(|v| v.get("detail"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            Some("unsupported") => Some("unsupported".into()),
            _ => None,
        };
        if new_status == "pending" {
            row.attempts += 1;
            row.next_attempt = seconds() + [2, 5, 15, 60][row.attempts.min(4) - 1];
        }
        let input = row.input_ref();
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
            {
                let mut inner = self.inner.lock().expect("room lock");
                let now = std::time::Instant::now();
                for sid in inner.browsers.ids() {
                    inner.expire_playback(&sid, now);
                    inner.dispatch_client(&sid);
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}
