//! Reports a connector sends about a live binding: working state, engine and read receipts.
use serde_json::{json, Value};

use super::declaration::engine;
use super::latency::{latency_now_micros, LatencyEvent};
use super::util::field;
use super::Room;

impl Room {
    pub fn working(&self, cid: &str, data: &Value) {
        let bid = field(data, "binding_id");
        let mut inner = self.inner.lock().expect("room lock");
        let Some(thread) = inner.bindings.live_of(cid, bid).map(|b| b.thread.clone()) else {
            return;
        };
        let Some(working) = data.get("working").and_then(Value::as_bool) else {
            return;
        };
        inner.bindings.set_working(&thread, working);
        let Some(b) = inner.bindings.live_of(cid, bid) else {
            return;
        };
        for (_, c) in inner.browsers.on_thread(&b.thread) {
            let mut out = json!({"thread_id":b.thread,"working":working});
            if let Some(obj) = out.as_object_mut() {
                for key in ["turn_id", "turn_phase", "session_id", "revision"] {
                    if let Some(v) = data.get(key) {
                        obj.insert(key.into(), v.clone());
                    }
                }
            }
            c.notify(json!({"type":"voice-conversation","data":out}));
        }
    }
    pub fn engine(&self, cid: &str, data: &Value) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(b) = inner.bindings.live_of_mut(cid, field(data, "binding_id")) {
            if let Some(e) = engine(data.get("engine")) {
                if e.get("model").is_some() {
                    b.engine = Some(e);
                }
            }
        }
    }
    pub fn read(&self, cid: &str, data: &Value) {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let Some(b) = inner.bindings.live_of(cid, field(data, "binding_id")) else {
            return;
        };
        let thread = b.thread.clone();
        let Some(input) = inner.journal.mark_read(&thread, field(data, "message_id")) else {
            return;
        };
        inner.browsers.input_receipt(&input, "read");
        inner.mark_latency(
            &input.session,
            &thread,
            input.revision,
            None,
            LatencyEvent::Read,
            latency_now_micros(),
        );
    }
}
