//! Bindings: a connector's registration of one agent thread, and the set of them by ID.
use std::collections::HashMap;

use serde_json::{json, Value};

use super::declaration::Declaration;
use super::util::seconds;

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
    fn new(id: &str, connector: &str, thread: &str) -> Self {
        Self {
            id: id.into(),
            connector: connector.into(),
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
        }
    }
    /// Take a fresh registration: the binding is active and live again, and what the connector
    /// declared replaces what it declared before, except that an absent title, inbound report
    /// or engine keeps the previous one.
    pub(super) fn renew(&mut self, declared: Declaration, pull_input: bool) {
        self.harness = declared.harness;
        self.active = true;
        self.live = true;
        if declared.title.is_some() {
            self.title = declared.title;
        }
        if declared.inbound.is_some() {
            self.inbound = declared.inbound;
        }
        self.capabilities = declared.capabilities;
        if declared.engine.is_some() {
            self.engine = declared.engine;
        }
        self.route = declared.route;
        self.pull_input = pull_input;
    }
    pub(super) fn end(&mut self) {
        self.active = false;
        self.live = false;
    }
    pub(super) fn view(&self) -> Value {
        json!({"id": self.id, "connector": self.connector, "thread": self.thread,
        "harness": self.harness, "title": self.title, "created": self.created, "active": self.active as u8,
        "inbound": self.inbound, "capabilities": self.capabilities, "engine": self.engine, "route": self.route,
        "input_mode": if self.pull_input { "pull" } else { "push" },
        "connected": self.live})
    }
}

#[derive(Default)]
pub(super) struct Bindings {
    by_id: HashMap<String, Binding>,
    /// What each thread's harness last said about working, for a call that lands on it mid-turn.
    working: HashMap<String, bool>,
}
impl Bindings {
    pub(super) fn set_working(&mut self, thread: &str, working: bool) {
        self.working.insert(thread.to_owned(), working);
    }
    pub(super) fn working(&self, thread: &str) -> Option<bool> {
        self.working.get(thread).copied()
    }
    /// Connector `cid` is gone: a conversation nobody can reach is not working any more.
    pub(super) fn forget_working_of(&mut self, cid: &str) {
        for binding in self.by_id.values().filter(|b| b.connector == cid && b.live) {
            self.working.remove(&binding.thread);
        }
    }
    pub(super) fn get(&self, bid: &str) -> Option<&Binding> {
        self.by_id.get(bid)
    }
    pub(super) fn get_mut(&mut self, bid: &str) -> Option<&mut Binding> {
        self.by_id.get_mut(bid)
    }
    /// The binding `bid` if connector `cid` owns it and it is live.
    pub(super) fn live_of(&self, cid: &str, bid: &str) -> Option<&Binding> {
        self.by_id.get(bid).filter(|b| b.connector == cid && b.live)
    }
    pub(super) fn live_of_mut(&mut self, cid: &str, bid: &str) -> Option<&mut Binding> {
        self.by_id
            .get_mut(bid)
            .filter(|b| b.connector == cid && b.live)
    }
    /// The binding `bid`, created for connector `cid` on `thread` if it does not exist yet.
    pub(super) fn get_or_create(&mut self, bid: &str, cid: &str, thread: &str) -> &mut Binding {
        self.by_id
            .entry(bid.to_owned())
            .or_insert_with(|| Binding::new(bid, cid, thread))
    }
    pub(super) fn active(&self) -> impl Iterator<Item = &Binding> {
        self.by_id.values().filter(|b| b.active)
    }
    /// The newest active binding of a thread, live or not.
    pub(super) fn newest_active(&self, thread: &str) -> Option<&Binding> {
        self.active()
            .filter(|b| b.thread == thread)
            .max_by_key(|b| b.created)
    }
    /// The newest active binding connector `cid` holds on a thread.
    pub(super) fn newest_active_of(&self, cid: &str, thread: &str) -> Option<&Binding> {
        self.by_id
            .values()
            .filter(|b| b.connector == cid && b.thread == thread && b.active)
            .max_by_key(|b| b.created)
    }
    /// The live binding a thread's input goes to first: the newest, ties broken by ID so push
    /// and pull agree on it.
    pub(super) fn delivery_target(&self, thread: &str) -> Option<&Binding> {
        self.by_id
            .values()
            .filter(|b| b.active && b.live && b.thread == thread)
            .max_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)))
    }
    /// The newest live push binding of a thread that is not `busy` with another delivery.
    pub(super) fn push_target(
        &self,
        thread: &str,
        busy: impl Fn(&str) -> bool,
    ) -> Option<&Binding> {
        self.by_id
            .values()
            .filter(|b| b.active && b.live && b.thread == thread && !b.pull_input && !busy(&b.id))
            .max_by_key(|b| b.created)
    }
    /// Connector `cid` lost the connection its active bindings used; returns their IDs.
    pub(super) fn go_offline_active(&mut self, cid: &str) -> Vec<String> {
        self.go_offline_where(|b| b.connector == cid && b.active)
    }
    /// Connector `cid` is gone: none of its bindings is live; returns all their IDs.
    pub(super) fn go_offline(&mut self, cid: &str) -> Vec<String> {
        self.go_offline_where(|b| b.connector == cid)
    }
    fn go_offline_where(&mut self, affected: impl Fn(&Binding) -> bool) -> Vec<String> {
        self.by_id
            .values_mut()
            .filter(|b| affected(b))
            .map(|b| {
                b.live = false;
                b.id.clone()
            })
            .collect()
    }
}
