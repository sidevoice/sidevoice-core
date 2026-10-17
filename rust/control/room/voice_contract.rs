//! Opt-in voice message contract. The journal remains the one source of delivered input and
//! written replies; this state holds only process-local admission, attempts, and physical facts.
use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::error::RoomError;
use super::journal::Row;
use super::util::{field, id, millis, seconds};
use super::{Inner, Room};

const MAX_PENDING: usize = 8;
const AUTO_TTL: u64 = 300;
const ORIGINAL_TTL: u64 = 1800;
const MAX_ORIGINALS: usize = 128;
const MAX_ORIGINAL_BYTES: usize = 2 * 1024 * 1024;
const MAX_IDEMPOTENCY_KEYS: usize = if cfg!(test) { 256 } else { 65_536 };
const MAX_PENDING_FACTS: usize = 4096;

#[derive(Clone)]
struct Original {
    utterance_id: String,
    binding_id: String,
    thread: String,
    row_id: String,
    text: String,
    created: u64,
    result: Value,
}

#[derive(Clone)]
struct Attempt {
    id: String,
    original: String,
    thread: String,
    binding_id: String,
    epoch: u64,
    listeners: Vec<(String, String)>, // session and exact focus identity at admission
    start_chars: u64,
    deadline: u64,
    requested: bool,
    auto_resume: bool,
    typed_fenced: bool,
    state: &'static str,
    terminal: HashMap<String, String>,
}

#[derive(Clone, Default)]
struct Heard {
    confirmed: Option<u64>,
    complete: bool,
}

struct PendingHearing {
    original: String,
    listener: String,
    fact: Value,
}

pub(super) struct VoiceContract {
    instance: String,
    originals: HashMap<String, Original>,
    original_order: VecDeque<String>,
    event_keys: HashMap<String, (Value, Value)>,
    original_keys: HashMap<String, (Value, Value)>,
    replay_keys: HashMap<String, (Value, Value)>,
    attempts: HashMap<String, Attempt>,
    attempt_order: VecDeque<String>,
    pending: VecDeque<String>,
    hearing: HashMap<(String, String), Heard>,
    changes: HashMap<String, Vec<PendingHearing>>,
    changes_truncated: HashSet<String>,
    outcomes: HashMap<String, Vec<Value>>,
    outcomes_truncated: HashSet<String>,
    initial_sessions: HashMap<String, (String, u64)>,
    return_noted: HashSet<(String, String)>,
    recovery_handed_off: HashSet<String>,
}

impl Default for VoiceContract {
    fn default() -> Self {
        Self {
            instance: id(),
            originals: HashMap::new(),
            original_order: VecDeque::new(),
            event_keys: HashMap::new(),
            original_keys: HashMap::new(),
            replay_keys: HashMap::new(),
            attempts: HashMap::new(),
            attempt_order: VecDeque::new(),
            pending: VecDeque::new(),
            hearing: HashMap::new(),
            changes: HashMap::new(),
            changes_truncated: HashSet::new(),
            outcomes: HashMap::new(),
            outcomes_truncated: HashSet::new(),
            initial_sessions: HashMap::new(),
            return_noted: HashSet::new(),
            recovery_handed_off: HashSet::new(),
        }
    }
}

fn rejected(reason: &str, event_id: &str, uid: &str) -> Value {
    json!({"status":"rejected","reason":reason,"event_id":event_id,"utterance_id":uid})
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 200
}

fn fingerprint(parts: Value) -> Value {
    json!(format!(
        "{:x}",
        Sha256::digest(parts.to_string().as_bytes())
    ))
}

impl VoiceContract {
    #[cfg(test)]
    pub(super) fn attempt_state(&self, aid: &str) -> Option<&str> {
        self.attempts.get(aid).map(|attempt| attempt.state)
    }
    #[cfg(test)]
    pub(super) fn force_deadline(&mut self, aid: &str) {
        if let Some(attempt) = self.attempts.get_mut(aid) {
            attempt.deadline = 0;
        }
    }
    #[cfg(test)]
    pub(super) fn age_original(&mut self, uid: &str) {
        if let Some(original) = self.originals.get_mut(uid) {
            original.created = 0;
        }
    }
    pub(super) fn instance(&self) -> &str {
        &self.instance
    }
    pub(super) fn initial_session(&mut self, bid: &str, epoch: u64) -> String {
        let entry = self
            .initial_sessions
            .entry(bid.to_owned())
            .or_insert_with(|| (format!("binding:{bid}:{}", id()), epoch));
        entry.1 = epoch;
        entry.0.clone()
    }
    fn instance_matches(&self, bid: &str, p: &Value) -> bool {
        self.initial_sessions.contains_key(bid)
            && p["voice_contract_instance"].as_str() == Some(self.instance.as_str())
    }
    fn cleanup(&mut self, now: u64) -> Vec<Attempt> {
        let timed_out: Vec<Attempt> = self
            .attempt_order
            .iter()
            .filter_map(|aid| self.attempts.get(aid))
            .filter(|a| matches!(a.state, "pending" | "queued" | "fenced") && a.deadline <= now)
            .cloned()
            .collect();
        for attempt in &timed_out {
            self.end_attempt(&attempt.id, "not_started", Some("expired"));
        }
        timed_out
    }

    fn expire_originals(&mut self, now: u64) {
        let expired: Vec<String> = self
            .originals
            .iter()
            .filter(|(uid, original)| {
                now.saturating_sub(original.created) >= ORIGINAL_TTL
                    && !self
                        .attempts
                        .values()
                        .any(|attempt| &attempt.original == *uid && attempt.state != "terminal")
            })
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in expired {
            self.remove_original(&uid);
        }
    }

    fn remove_original(&mut self, uid: &str) {
        self.originals.remove(uid);
        self.original_order.retain(|candidate| candidate != uid);
        self.hearing.retain(|(original, _), _| original != uid);
        for pending in self.changes.values_mut() {
            pending.retain(|change| change.original != uid);
        }
        self.recovery_handed_off.remove(uid);
    }

    fn reserve_original(&mut self, bytes: usize) -> bool {
        let mut used: usize = self
            .originals
            .values()
            .map(|original| original.text.len())
            .sum();
        while self.originals.len() >= MAX_ORIGINALS || used + bytes > MAX_ORIGINAL_BYTES {
            let Some(uid) = self
                .original_order
                .iter()
                .find(|uid| {
                    !self
                        .attempts
                        .values()
                        .any(|a| &a.original == *uid && a.state != "terminal")
                })
                .cloned()
            else {
                return false;
            };
            used = used.saturating_sub(self.originals.get(&uid).map_or(0, |o| o.text.len()));
            self.remove_original(&uid);
        }
        true
    }

    fn end_attempt(&mut self, aid: &str, state: &str, reason: Option<&str>) {
        let Some(attempt) = self.attempts.get_mut(aid) else {
            return;
        };
        if attempt.state == "terminal" {
            return;
        }
        attempt.state = "terminal";
        let thread = attempt.thread.clone();
        let original = attempt.original.clone();
        self.pending.retain(|candidate| candidate != aid);
        let outcomes = self.outcomes.entry(thread.clone()).or_default();
        if outcomes.len() >= MAX_PENDING_FACTS {
            outcomes.remove(0);
            self.outcomes_truncated.insert(thread);
        }
        outcomes.push(json!({"utterance_id":original,
            "attempt_id":aid,"state":state,"reason":reason}));
    }

    fn active_count(&self, thread: &str) -> usize {
        self.attempts
            .values()
            .filter(|a| a.thread == thread && a.state != "terminal")
            .count()
    }
    fn insert_attempt(&mut self, attempt: Attempt) -> bool {
        while self.attempts.len() >= 4096 {
            let Some(index) = self
                .attempt_order
                .iter()
                .position(|aid| self.attempts.get(aid).is_none_or(|a| a.state == "terminal"))
            else {
                return false;
            };
            if let Some(aid) = self.attempt_order.remove(index) {
                self.attempts.remove(&aid);
            }
        }
        self.attempt_order.push_back(attempt.id.clone());
        self.attempts.insert(attempt.id.clone(), attempt);
        true
    }
    fn can_insert_attempt(&self) -> bool {
        self.attempts.len() < 4096 || self.attempts.values().any(|a| a.state == "terminal")
    }

    fn remember_event(&mut self, key: &str, fingerprint: Value, result: Value) {
        self.event_keys.insert(key.into(), (fingerprint, result));
    }

    fn remember_original(&mut self, key: &str, fingerprint: Value, result: Value) {
        self.original_keys.insert(key.into(), (fingerprint, result));
    }

    fn remember_replay(&mut self, key: &str, fingerprint: Value, result: Value) {
        self.replay_keys.insert(key.into(), (fingerprint, result));
    }
}

mod api;
mod attempts;
mod evidence;
mod recovery;

pub(super) use evidence::{sentence_start, validated_not_played, validated_snapshot};
