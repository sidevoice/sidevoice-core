//! Participants: the conversations a call can talk to, and how reachable each one is.
use serde_json::{json, Value};

use super::bindings::Binding;
use super::util::default_title;
use super::Room;

impl Room {
    pub fn participants(&self, session: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let current = session.and_then(|sid| inner.browsers.get(sid));
        let selected = current
            .and_then(|b| b.target.as_ref())
            .map(|t| t.thread.as_str());
        let language = current.map_or("en", |b| b.language.as_str());
        json!(inner.bindings.active().map(|b| {
            let host=inner.credentials.get(&b.connector).and_then(|c| c.get("host"));
            json!({"thread_id":b.thread,"title":b.title.clone().unwrap_or_else(||default_title(&b.thread,language)),
                "harness":b.harness,"available":b.live,"machine":{"id":b.connector,"host":host},
                "capabilities":b.capabilities,"engine":b.engine,"route":b.route,
                "reach":reachability(b,language),"selected":selected==Some(b.thread.as_str())})
        }).collect::<Vec<_>>())
    }
}

fn reachability(binding: &Binding, language: &str) -> Value {
    let render = |key: &'static str| {
        crate::messages::render(&crate::messages::LocalizedMessage::new(key), language)
    };
    if !binding.live {
        return json!({"state":"offline","detail":render("room.reach_offline")});
    }
    if binding
        .inbound
        .as_ref()
        .is_some_and(|inbound| inbound.get("ok") == Some(&Value::Bool(false)))
    {
        return json!({"state":"holding","detail":render("room.reach_holding"),"remedy":Value::Null});
    }
    if binding.capabilities.get("deliver").and_then(Value::as_str) == Some("unsupported") {
        return json!({"state":"holding","detail":render("room.reach_unsupported")});
    }
    json!({"state":"listening","detail":Value::Null})
}
