//! Push delivery: handing pending input to a connector and settling its acknowledgement.
use std::time::Duration;

use serde_json::{json, Value};

use super::journal::INPUT_TTL;
use super::latency::{latency_now_micros, mark_latency, LatencyEvent};
use super::peers::{ConnectorPeer, PeerError};
use super::playback::dispatch_client;
use super::pull::delivery_binding;
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
        let mut inner = self.inner.lock().expect("room lock");
        let now = seconds();
        let mut work = Vec::new();
        for ix in 0..inner.rows.len() {
            let row = &inner.rows[ix];
            if row.role != "user" || row.status != "pending" {
                continue;
            }
            let rid = row.id.clone();
            let thread = row.thread.clone();
            let queued = row.queued_at;
            let next = row.next_attempt;
            if now.saturating_sub(queued) >= INPUT_TTL {
                inner.rows[ix].status = "not_sent".into();
                inner.rows[ix].reason = Some("expired".into());
                let payload = inner.rows[ix].payload.clone().unwrap_or_default();
                let sid = inner.rows[ix].session.clone();
                if let Some(c) = inner.browsers.get(&sid) {
                    let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":rid,"thread_id":thread,"session_id":sid,"status":"not_sent"}}));
                }
                continue;
            }
            if next > now {
                continue;
            }
            // A pull binding consumes the room journal directly. Falling back to an older
            // push binding here would deliver the same words twice.
            if delivery_binding(&inner, &thread).is_some_and(|b| b.pull_input) {
                continue;
            }
            let Some(b) = inner
                .bindings
                .values()
                .filter(|b| {
                    b.active
                        && b.live
                        && b.thread == thread
                        && !b.pull_input
                        && !inner.inflight.contains_key(&b.id)
                })
                .max_by_key(|b| b.created)
            else {
                continue;
            };
            let bid = b.id.clone();
            let Some(peer) = inner.peers.get(&b.connector).cloned() else {
                continue;
            };
            let row = &inner.rows[ix];
            let payload = row.payload.as_ref().unwrap();
            let data = json!({"event_id":rid,"binding_id":bid,"thread":thread,"text":row.text,"channel":"voice","session_id":payload["session_id"],"revision":payload["revision"],"message_id":payload["message_id"]});
            inner.rows[ix].status = "sending".into();
            inner.inflight.insert(bid.clone(), rid.clone());
            work.push((bid, rid, peer, data));
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
        let mut inner = self.inner.lock().expect("room lock");
        if inner.inflight.get(bid).is_none_or(|id| id != rid) {
            return;
        }
        let Some(b) = inner.bindings.get(bid) else {
            return;
        };
        if inner
            .peers
            .get(&b.connector)
            .is_none_or(|p| p.generation != generation)
        {
            return;
        }
        inner.inflight.remove(bid);
        let Some(row) = inner.rows.iter_mut().find(|r| r.id == rid) else {
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
        let sid = row.session.clone();
        let payload = row.payload.clone().unwrap_or_default();
        if let Some(c) = inner.browsers.get_mut(&sid) {
            if new_status == "delivered" {
                c.sent += 1;
            }
            let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":rid,"thread_id":payload["thread_id"],"session_id":sid,"status":new_status}}));
        }
        if new_status == "delivered" {
            if let (Some(thread), Some(revision)) =
                (payload["thread_id"].as_str(), payload["revision"].as_u64())
            {
                mark_latency(
                    &mut inner,
                    &sid,
                    thread,
                    revision,
                    None,
                    LatencyEvent::DeliveryAccepted,
                    latency_now_micros(),
                );
            }
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
                let clients: Vec<String> = inner.browsers.keys().cloned().collect();
                for sid in clients {
                    dispatch_client(&mut inner, &sid);
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}
