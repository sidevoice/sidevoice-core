//! Participants: the conversations a call can talk to, and how reachable each one is.
use std::collections::HashSet;

use serde_json::{json, Value};

use super::bindings::Binding;
use super::error::RoomError;
use super::util::default_title;
use super::Room;

impl Room {
    pub fn participants(&self, session: Option<&str>) -> Value {
        self.participant_inventory(session)["participants"].clone()
    }
    /// Rows and deliberate closures are one snapshot. Missing rows alone never prove a leave.
    pub fn participant_inventory(&self, session: Option<&str>) -> Value {
        self.participant_inventory_for(session, None, &HashSet::new())
            .expect("no device, nothing to write")
    }
    /// The same snapshot as seen by `device`: it leaves out what that device removed, and the device is noted as
    /// showing the rest. `recent` are the paired devices seen in the last 30 days (the device registry's knowledge).
    pub fn participant_inventory_for(
        &self,
        session: Option<&str>,
        device: Option<&str>,
        recent: &HashSet<String>,
    ) -> Result<Value, RoomError> {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if let Some(device) = device {
            let threads: Vec<_> = inner.bindings.active().map(|b| b.thread.clone()).collect();
            for thread in threads {
                inner.shown.note(&thread, device, recent);
            }
        }
        // A viewer is answered only once it is durably a viewer; a write that fails stays dirty and is an error the
        // device asks again after.
        if device.is_some() && inner.shown.dirty() && self.save(inner).is_err() {
            return Err(RoomError::new(500, "room.state_save_failed"));
        }
        let inner = &*inner;
        let current = session.and_then(|sid| inner.browsers.get(sid));
        let selected = current
            .and_then(|b| b.target.as_ref())
            .map(|t| t.thread.as_str());
        let language = current.map_or("en", |b| b.language.as_str());
        let participants = inner.bindings.active().filter(|b| !device.is_some_and(|d| inner.shown.hidden_from(&b.thread, d))).map(|b| {
            let host = inner.credentials.get(&b.connector).and_then(|c| c.get("host"));
            json!({"thread_id":b.thread,"title":b.title.clone().unwrap_or_else(||default_title(&b.thread,language)),
                "harness":b.harness,"available":b.live,"machine":{"id":b.connector,"host":host},
                "capabilities":b.capabilities,"engine":b.engine,"route":b.route,
                "reach":reachability(b,language),"selected":selected==Some(b.thread.as_str()),
                "epoch":inner.conversations.epoch(&b.thread),
                "unheard":inner.journal.unheard_summary(&b.thread)
                    .map_or(Value::Null, |(count,since)| json!({"count":count,"since":since})),
                "working":inner.bindings.working(&b.thread)})
        }).collect::<Vec<_>>();
        let mut closed_threads: Vec<_> = inner.closed_threads.iter().cloned().collect();
        closed_threads.sort();
        Ok(json!({"participants":participants,"closed_threads":closed_threads}))
    }
}

fn reachability(binding: &Binding, language: &str) -> Value {
    let render = |key: &'static str| {
        crate::messages::render(&crate::messages::LocalizedMessage::new(key), language)
    };
    if !binding.live {
        return json!({"state":"offline","detail":render("room.reach_offline")});
    }
    if let Some(inbound) = binding
        .inbound
        .as_ref()
        .filter(|inbound| inbound.get("ok") == Some(&Value::Bool(false)))
    {
        // The connector knows why its harness holds input and how to fix it: say that, not ours.
        let detail = inbound
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
            .map_or_else(|| render("room.reach_holding"), str::to_owned);
        let remedy = inbound.get("remedy").cloned().unwrap_or(Value::Null);
        return json!({"state":"holding","detail":detail,"remedy":remedy});
    }
    if binding.capabilities.get("deliver").and_then(Value::as_str) == Some("unsupported") {
        return json!({"state":"holding","detail":render("room.reach_unsupported")});
    }
    json!({"state":"listening","detail":Value::Null})
}
