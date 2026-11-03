//! Which devices still show each conversation. A device removing a conversation only removes it from itself;
//! the room closes the conversation when no device that is still paired and was seen in the last 30 days shows it
//! any more. Whether a device counts is the device registry's knowledge (its last contact, any request): the room is
//! handed that set (`recent`) and keeps only the memberships.
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::util::valid_thread;

#[derive(Default)]
pub(super) struct Shown {
    /// Per conversation, each device that has it and whether that device removed it.
    threads: HashMap<String, HashMap<String, bool>>,
    /// A change not yet written; every later save retries it.
    dirty: Cell<bool>,
}

impl Shown {
    pub(super) fn from_state(state: &Value) -> Self {
        let mut threads = HashMap::new();
        for (thread, devices) in state
            .get("conversation_devices")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(thread, _)| valid_thread(thread))
        {
            let devices = devices
                .as_object()
                .into_iter()
                .flatten()
                .map(|(device, row)| {
                    let hidden = row.get("hidden").and_then(Value::as_bool).unwrap_or(false);
                    (device.clone(), hidden)
                })
                .collect();
            threads.insert(thread.clone(), devices);
        }
        Self {
            threads,
            dirty: Cell::new(false),
        }
    }
    pub(super) fn state(&self) -> Value {
        let threads: serde_json::Map<String, Value> = self
            .threads
            .iter()
            .map(|(thread, devices)| {
                let devices: serde_json::Map<String, Value> = devices
                    .iter()
                    .map(|(device, hidden)| (device.clone(), json!({"hidden": hidden})))
                    .collect();
                (thread.clone(), Value::Object(devices))
            })
            .collect();
        Value::Object(threads)
    }
    pub(super) fn dirty(&self) -> bool {
        self.dirty.get()
    }
    pub(super) fn saved(&self) {
        self.dirty.set(false);
    }
    /// A conversation with no record yet is shown by every device that counts: nothing is known to have removed it.
    fn record(&mut self, thread: &str, recent: &HashSet<String>) -> &mut HashMap<String, bool> {
        let dirty = &self.dirty;
        self.threads.entry(thread.to_owned()).or_insert_with(|| {
            dirty.set(true);
            recent
                .iter()
                .map(|device| (device.clone(), false))
                .collect()
        })
    }
    /// `device` is looking at `thread`.
    pub(super) fn note(&mut self, thread: &str, device: &str, recent: &HashSet<String>) {
        let known = self.record(thread, recent);
        if !known.contains_key(device) {
            known.insert(device.to_owned(), false);
            self.dirty.set(true);
        }
    }
    pub(super) fn hidden_from(&self, thread: &str, device: &str) -> bool {
        self.threads
            .get(thread)
            .and_then(|devices| devices.get(device))
            .copied()
            .unwrap_or(false)
    }
    /// `device` removes `thread` from itself.
    pub(super) fn hide(&mut self, thread: &str, device: &str, recent: &HashSet<String>) {
        self.record(thread, recent).insert(device.to_owned(), true);
        self.dirty.set(true);
    }
    /// Whether any device that counts still shows `thread`.
    pub(super) fn any_showing(&self, thread: &str, recent: &HashSet<String>) -> bool {
        self.threads.get(thread).is_some_and(|devices| {
            devices
                .iter()
                .any(|(device, hidden)| !hidden && recent.contains(device))
        })
    }
    /// Conversations with a record that no device that counts shows any more.
    pub(super) fn departed(&self, recent: &HashSet<String>) -> Vec<String> {
        self.threads
            .keys()
            .filter(|thread| !self.any_showing(thread, recent))
            .cloned()
            .collect()
    }
    /// The agent connected voice anew: every removal of the conversation is undone; who still shows it keeps doing so.
    pub(super) fn restore(&mut self, thread: &str) {
        if let Some(devices) = self.threads.get_mut(thread) {
            if devices.values().any(|hidden| *hidden) {
                devices.values_mut().for_each(|hidden| *hidden = false);
                self.dirty.set(true);
            }
        }
    }
    /// The conversation closed: its devices start over. The record is handed back so a close that fails to be written can
    /// put it back (`put`).
    pub(super) fn forget(&mut self, thread: &str) -> Option<HashMap<String, bool>> {
        let record = self.threads.remove(thread);
        if record.is_some() {
            self.dirty.set(true);
        }
        record
    }
    pub(super) fn put(&mut self, thread: &str, record: HashMap<String, bool>) {
        self.threads.insert(thread.to_owned(), record);
        self.dirty.set(true);
    }
}
