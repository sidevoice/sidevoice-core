//! Reports a connector sends about a live binding: working state, engine and read receipts.
use serde_json::{json, Value};

use super::capabilities::engine;
use super::latency::{latency_now_micros, mark_latency, LatencyEvent};
use super::util::field;
use super::Room;

impl Room {
    pub fn working(&self, cid: &str, data: &Value) {
        let bid = field(data, "binding_id");
        let mut inner = self.inner.lock().expect("room lock");
        let Some(b) = inner
            .bindings
            .get(bid)
            .filter(|b| b.connector == cid && b.live)
        else {
            return;
        };
        let thread = b.thread.clone();
        let Some(working) = data.get("working").and_then(Value::as_bool) else {
            return;
        };
        inner.working.insert(thread.clone(), working);
        for c in inner
            .browsers
            .values()
            .filter(|c| c.target.as_ref().is_some_and(|t| t.thread == thread))
        {
            let mut out = json!({"thread_id":thread,"working":working});
            if let Some(obj) = out.as_object_mut() {
                for key in ["turn_id", "turn_phase", "session_id", "revision"] {
                    if let Some(v) = data.get(key) {
                        obj.insert(key.into(), v.clone());
                    }
                }
            }
            let _ = c
                .sender
                .try_send(json!({"type":"voice-conversation","data":out}));
        }
    }
    pub fn engine(&self, cid: &str, data: &Value) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(b) = inner
            .bindings
            .get_mut(field(data, "binding_id"))
            .filter(|b| b.connector == cid && b.live)
        {
            if let Some(e) = engine(data.get("engine")) {
                if e.get("model").is_some() {
                    b.engine = Some(e);
                }
            }
        }
    }
    pub fn read(&self, cid: &str, data: &Value) {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(b) = inner
            .bindings
            .get(field(data, "binding_id"))
            .filter(|b| b.connector == cid && b.live)
        else {
            return;
        };
        let thread = b.thread.clone();
        let mid = field(data, "message_id");
        if let Some(row) = inner.rows.iter_mut().rev().find(|r| {
            r.thread == thread
                && r.role == "user"
                && r.payload
                    .as_ref()
                    .is_some_and(|p| field(p, "message_id") == mid)
                && !matches!(r.status.as_str(), "read" | "not_sent")
        }) {
            row.status = "read".into();
            let sid = row.session.clone();
            let payload = row.payload.clone().unwrap_or_default();
            if let Some(c) = inner.browsers.get(&sid) {
                let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":payload["history_id"],"thread_id":thread,"session_id":sid,"status":"read"}}));
            }
            if let Some(revision) = payload["revision"].as_u64() {
                mark_latency(
                    &mut inner,
                    &sid,
                    &thread,
                    revision,
                    None,
                    LatencyEvent::Read,
                    latency_now_micros(),
                );
            }
        }
    }
}
