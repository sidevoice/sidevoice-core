//! Push delivery: handing pending input to a connector and settling its acknowledgement.
use std::time::Duration;

use serde_json::{json, Value};

use super::latency::{latency_now_micros, LatencyEvent};
use super::peers::{ConnectorPeer, PeerError};
use super::util::{field, seconds};
use super::{Inner, Room};
use crate::control::telemetry::Counted;

const ACK_TIMEOUT: Duration = Duration::from_secs(60);
/// Seconds before another attempt, by attempts made so far.
const RETRY_SECONDS: [u64; 4] = [2, 5, 15, 60];
/// Attempts after which a note is given up.
const NOTE_ATTEMPTS: usize = 5;

fn retry_after(attempts: usize) -> u64 {
    RETRY_SECONDS[attempts.clamp(1, RETRY_SECONDS.len()) - 1]
}

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
            let mut data = json!({"event_id":row.id,"binding_id":binding.id,"thread":row.thread,"text":row.text,"channel":"voice","session_id":payload["session_id"],"revision":payload["revision"],"message_id":payload["message_id"]});
            if let Some(unheard) = payload.get("unheard") {
                data["unheard"] = unheard.clone();
            }
            row.status = "sending".into();
            inner.inflight.start(&binding.id, &row.id);
            work.push((binding.id.clone(), row.id.clone(), peer, data));
        }
        work.extend(inner.due_notes(now));
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
        let status = answer.as_ref().ok().and_then(valid_delivery_ack);
        if let Some(thread) = inner.unheard.note_thread(rid) {
            settle_note(inner, &thread, status);
            return;
        }
        let Some(row) = inner.journal.find_mut(rid) else {
            return;
        };
        if row.status == "read" {
            return;
        }
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

impl Inner {
    /// The notes due now, each to its conversation's push binding: `input.deliver` on the
    /// `note` channel, no words of the person's, and what they did not hear. A note for a call
    /// that left its conversation is dropped; the list waits for the next message there.
    fn due_notes(&mut self, now: u64) -> Vec<(String, String, ConnectorPeer, Value)> {
        let mut work = Vec::new();
        for thread in self.unheard.due_notes(now) {
            let pull = self
                .bindings
                .delivery_target(&thread)
                .is_some_and(|b| b.pull_input);
            let Some(session) = self.unheard.note_mut(&thread).map(|n| n.session.clone()) else {
                continue;
            };
            let Some(revision) = self
                .browsers
                .get(&session)
                .filter(|browser| browser.is_on(&thread))
                .map(|browser| browser.revision)
            else {
                self.unheard.drop_note(&thread);
                continue;
            };
            let untold = self
                .unheard
                .note_mut(&thread)
                .is_some_and(|n| n.told.is_none());
            if pull || (untold && !self.unheard.has(&thread)) {
                // A pull binding gets the list with the next message it fetches; a message sent
                // meanwhile already took it.
                self.unheard.drop_note(&thread);
                continue;
            }
            let Some((bid, peer)) = self
                .bindings
                .push_target(&thread, |bid| self.inflight.is_busy(bid))
                .and_then(|b| Some((b.id.clone(), self.peers.get(&b.connector).cloned()?)))
            else {
                continue;
            };
            let told = match self.unheard.note_mut(&thread).and_then(|n| n.told.clone()) {
                Some(told) => told,
                None => match self.unheard.take(&thread, &self.journal) {
                    Some(told) => told,
                    None => {
                        self.unheard.drop_note(&thread);
                        continue;
                    }
                },
            };
            let note = self.unheard.note_mut(&thread).expect("note present");
            note.told = Some(told.clone());
            note.due = u64::MAX;
            let data = json!({"event_id":note.id,"binding_id":bid,"thread":thread,"text":"","channel":"note",
                "session_id":session,"revision":revision,"message_id":note.id,"unheard":told});
            self.inflight.start(&bid, &note.id);
            work.push((bid, note.id.clone(), peer, data));
        }
        work
    }
}

/// A note the connector took (or will never take) is done; any other answer is tried again later,
/// up to [`NOTE_ATTEMPTS`].
fn settle_note(inner: &mut Inner, thread: &str, status: Option<&str>) {
    let Some(note) = inner.unheard.note_mut(thread) else {
        return;
    };
    note.attempts += 1;
    if matches!(status, Some("accepted" | "unknown" | "unsupported"))
        || note.attempts >= NOTE_ATTEMPTS
    {
        inner.unheard.drop_note(thread);
    } else {
        note.due = seconds() + retry_after(note.attempts);
    }
}
