//! Bindings: a connector's registration of one agent thread, how it is listed and how it ends.
use serde_json::{json, Value};

use super::capabilities::{capabilities, engine};
use super::error::RoomError;
use super::focus::Target;
use super::peers::ConnectorPeer;
use super::playback::interrupt_client;
use super::pull::release_pull_claims;
use super::util::{field, id, seconds, valid_thread};
use super::Room;

pub(super) struct Binding {
    pub(super) id: String,
    pub(super) connector: String,
    pub(super) thread: String,
    pub(super) harness: String,
    pub(super) title: Option<String>,
    pub(super) created: u64,
    pub(super) active: bool,
    pub(super) live: bool,
    pub(super) inbound: Option<Value>,
    pub(super) capabilities: Value,
    pub(super) engine: Option<Value>,
    pub(super) route: Option<String>,
    pub(super) pull_input: bool,
}
impl Binding {
    fn view(&self) -> Value {
        json!({"id": self.id, "connector": self.connector, "thread": self.thread,
        "harness": self.harness, "title": self.title, "created": self.created, "active": self.active as u8,
        "inbound": self.inbound, "capabilities": self.capabilities, "engine": self.engine, "route": self.route,
        "input_mode": if self.pull_input { "pull" } else { "push" },
        "connected": self.live})
    }
}

impl Room {
    pub fn binding_views(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        json!(inner
            .bindings
            .values()
            .filter(|b| b.active)
            .map(Binding::view)
            .collect::<Vec<_>>())
    }
    pub fn register(&self, cid: &str, data: &Value) -> Result<Value, RoomError> {
        let thread = field(data, "thread");
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.peers.contains_key(cid) {
            return Err(RoomError::new(409, "room.connector_disconnected"));
        }
        let requested = field(data, "binding_id");
        let pull_input = match field(data, "input_mode") {
            "" | "push" => false,
            "pull" => true,
            _ => return Err(RoomError::new(400, "room.input_mode_invalid")),
        };
        if let Some(existing) = inner.bindings.get(requested) {
            if existing.connector != cid {
                return Err(RoomError::new(409, "room.binding_foreign"));
            }
        }
        let existing = if !requested.is_empty() && inner.bindings.contains_key(requested) {
            Some(requested.to_owned())
        } else {
            inner
                .bindings
                .values()
                .filter(|b| b.connector == cid && b.thread == thread && b.active)
                .max_by_key(|b| b.created)
                .map(|b| b.id.clone())
        };
        let bid = existing.unwrap_or_else(id);
        let capabilities = capabilities(data.get("capabilities"), data.get("experimental"));
        let engine = engine(data.get("engine"));
        let route = match field(data, "route") {
            "cursor-editor-bridge" | "cursor-editor-view" | "cursor-cli-persist" | "cursor-cli" => {
                Some(field(data, "route").into())
            }
            _ => None,
        };
        let title = data
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(200).collect());
        let harness = data
            .get("harness")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .chars()
            .take(40)
            .collect();
        let inbound = data.get("inbound").filter(|v| v.is_object()).cloned();
        let binding = inner
            .bindings
            .entry(bid.clone())
            .or_insert_with(|| Binding {
                id: bid.clone(),
                connector: cid.into(),
                thread: thread.into(),
                harness: String::new(),
                title: None,
                created: seconds(),
                active: true,
                live: true,
                inbound: None,
                capabilities: Value::Null,
                engine: None,
                route: None,
                pull_input: false,
            });
        binding.harness = harness;
        binding.active = true;
        binding.live = true;
        if title.is_some() {
            binding.title = title;
        }
        if inbound.is_some() {
            binding.inbound = inbound;
        }
        binding.capabilities = capabilities;
        if engine.is_some() {
            binding.engine = engine;
        }
        binding.route = route;
        binding.pull_input = pull_input;
        let actual_thread = binding.thread.clone();
        inner.working.remove(&actual_thread);
        release_pull_claims(&mut inner, |row| {
            row.thread == actual_thread
                && (!pull_input || row.pull_claimed_by.as_deref() != Some(bid.as_str()))
        });
        Ok(
            json!({"client_ref": data.get("client_ref"), "binding_id": bid, "thread": actual_thread}),
        )
    }
    pub fn unregister(&self, cid: &str, bid: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(b) = inner.bindings.get_mut(bid).filter(|b| b.connector == cid) {
            b.active = false;
            b.live = false;
            let thread = b.thread.clone();
            inner.working.remove(&thread);
            release_pull_claims(&mut inner, |row| {
                row.pull_claimed_by.as_deref() == Some(bid)
            });
        }
    }
    pub fn close_channel(
        &self,
        thread: &str,
    ) -> Result<(Value, Option<(ConnectorPeer, Value)>), RoomError> {
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let binding_id = inner
            .bindings
            .values()
            .filter(|b| b.thread == thread && b.active)
            .max_by_key(|b| b.created)
            .map(|b| b.id.clone());
        if binding_id.is_none() && !inner.rows.iter().any(|r| r.thread == thread) {
            return Err(RoomError::new(409, "room.conversation_missing"));
        }
        let notify = binding_id
            .as_ref()
            .and_then(|bid| inner.bindings.get(bid))
            .and_then(|b| {
                inner.peers.get(&b.connector).cloned().map(|p| {
                    (
                        p,
                        json!({"binding_id":b.id,"thread":thread,"reason":"closed_from_room"}),
                    )
                })
            });
        if let Some(bid) = binding_id.as_ref() {
            if let Some(b) = inner.bindings.get_mut(bid) {
                b.active = false;
                b.live = false;
            }
            inner.inflight.remove(bid);
        }
        inner.working.remove(thread);
        let mut cancelled = Vec::new();
        for row in inner.rows.iter_mut().filter(|r| {
            r.role == "user"
                && r.thread == thread
                && matches!(r.status.as_str(), "pending" | "sending")
        }) {
            row.status = "not_sent".into();
            row.reason = Some("channel_closed".into());
            cancelled.push((
                row.id.clone(),
                row.session.clone(),
                row.payload.clone().unwrap_or_default(),
            ));
        }
        for (row_id, sid, payload) in cancelled {
            if let Some(browser) = inner.browsers.get(&sid) {
                let _ = browser
                    .sender
                    .try_send(json!({"type":"voice-input-receipt","data":{
                    "revision":payload["revision"],"history_id":row_id,"thread_id":thread,
                    "session_id":sid,"status":"not_sent"}}));
            }
        }
        let affected: Vec<String> = inner
            .browsers
            .iter()
            .filter(|(_, b)| b.target.as_ref().is_some_and(|t| t.thread == thread))
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in affected {
            if let Some(browser) = inner.browsers.get_mut(&sid) {
                browser.revision += 1;
                browser.target = Some(Target {
                    thread: String::new(),
                    title: None,
                    binding_id: id(),
                });
                browser.speaking = false;
                let _=browser.sender.try_send(json!({"type":"voice-cancel","data":{"session_id":sid,"revision":browser.revision}}));
            }
            interrupt_client(&mut inner, &sid, "focus_changed");
        }
        Ok((json!({"status":"closed","binding_id":binding_id}), notify))
    }
    pub fn participants(&self, session: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let current = session.and_then(|sid| inner.browsers.get(sid));
        let selected = current
            .and_then(|b| b.target.as_ref())
            .map(|t| t.thread.as_str());
        let language = current.map_or("en", |b| b.language.as_str());
        json!(inner.bindings.values().filter(|b| b.active).map(|b| {
            let host=inner.connectors.get(&b.connector).and_then(|c| c.get("host"));
            json!({"thread_id":b.thread,"title":b.title.clone().unwrap_or_else(||crate::messages::render(&crate::messages::LocalizedMessage::new("room.conversation_title").with_param("id",b.thread.chars().take(8).collect::<String>()),language)),
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
