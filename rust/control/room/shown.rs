//! Which devices still show each conversation. A device removing a conversation only removes it from itself;
//! the room closes the conversation when no device that was seen recently shows it any more.
use std::collections::HashMap;

use serde_json::{json, Value};

use super::util::{seconds, valid_thread};

/// A device that has not been seen for this long no longer keeps a conversation open.
const STALE_SECONDS: u64 = 30 * 24 * 3600;
/// How often a device's last-seen time is written again; the window is days, so an hour is plenty.
const REFRESH_SECONDS: u64 = 3600;

#[derive(Clone)]
struct Entry {
    seen: u64,
    /// The device removed the conversation: it stays out of the set and out of its inventory until the
    /// conversation is closed or reopened.
    hidden: bool,
}

#[derive(Default)]
pub(super) struct Shown {
    threads: HashMap<String, HashMap<String, Entry>>,
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
                .filter_map(|(device, row)| {
                    let seen = row.get("seen").and_then(Value::as_u64)?;
                    let hidden = row.get("hidden").and_then(Value::as_bool).unwrap_or(false);
                    Some((device.clone(), Entry { seen, hidden }))
                })
                .collect();
            threads.insert(thread.clone(), devices);
        }
        Self { threads }
    }
    pub(super) fn state(&self) -> Value {
        let threads: serde_json::Map<String, Value> = self
            .threads
            .iter()
            .map(|(thread, devices)| {
                let devices: serde_json::Map<String, Value> = devices
                    .iter()
                    .map(|(device, e)| {
                        (device.clone(), json!({"seen": e.seen, "hidden": e.hidden}))
                    })
                    .collect();
                (thread.clone(), Value::Object(devices))
            })
            .collect();
        Value::Object(threads)
    }
    /// `device` is looking at `thread`. True when the record changed enough to be written.
    pub(super) fn note(&mut self, thread: &str, device: &str) -> bool {
        let now = seconds();
        let devices = self.threads.entry(thread.to_owned()).or_default();
        match devices.get_mut(device) {
            Some(entry) if entry.hidden => false,
            Some(entry) if entry.seen + REFRESH_SECONDS > now => false,
            Some(entry) => {
                entry.seen = now;
                true
            }
            None => {
                devices.insert(
                    device.to_owned(),
                    Entry {
                        seen: now,
                        hidden: false,
                    },
                );
                true
            }
        }
    }
    pub(super) fn hidden_from(&self, thread: &str, device: &str) -> bool {
        self.threads
            .get(thread)
            .and_then(|devices| devices.get(device))
            .is_some_and(|entry| entry.hidden)
    }
    /// `device` removes `thread` from itself.
    pub(super) fn hide(&mut self, thread: &str, device: &str) {
        self.threads.entry(thread.to_owned()).or_default().insert(
            device.to_owned(),
            Entry {
                seen: seconds(),
                hidden: true,
            },
        );
    }
    /// Whether any device seen in the last 30 days still shows `thread`.
    pub(super) fn any_showing(&self, thread: &str) -> bool {
        let now = seconds();
        self.threads.get(thread).is_some_and(|devices| {
            devices
                .values()
                .any(|e| !e.hidden && e.seen + STALE_SECONDS > now)
        })
    }
    /// The conversation closed or reopened: its devices start over.
    pub(super) fn forget(&mut self, thread: &str) -> bool {
        self.threads.remove(thread).is_some()
    }
}
