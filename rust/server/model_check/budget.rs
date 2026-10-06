//! Admission for paid model checks: per-device and per-provider rate limits, one run per check,
//! and passed results remembered for a while.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::types::SpeechStage;

const WINDOW: Duration = Duration::from_secs(60);
const REMEMBER: Duration = Duration::from_secs(600);

#[derive(Default)]
pub(in crate::server) struct CheckBudget {
    inner: Mutex<BudgetState>,
}

#[derive(Default)]
struct BudgetState {
    windows: HashMap<(String, String), VecDeque<Instant>>,
    passed: HashMap<String, (Instant, Value)>,
    running: HashMap<String, watch::Sender<Option<Value>>>,
}

pub(super) enum Admission {
    /// A recent passing result for the same check, marked as remembered.
    Cached(Value),
    /// The same check is already running; wait for its result.
    Join(watch::Receiver<Option<Value>>),
    /// This caller runs the check and must `complete` it.
    Start(watch::Receiver<Option<Value>>),
    /// Seconds to wait, and whether the device or the provider limit was reached.
    Limited(u64, &'static str),
}

impl CheckBudget {
    pub(super) fn admit(&self, key: &str, device: &str, provider: &str) -> Admission {
        let now = Instant::now();
        let mut state = self.inner.lock().expect("check budget lock");
        state
            .passed
            .retain(|_, (at, _)| now.duration_since(*at) < REMEMBER);
        if let Some((_, result)) = state.passed.get(key) {
            let mut remembered = result.clone();
            remembered["remembered"] = json!(true);
            return Admission::Cached(remembered);
        }
        if let Some(running) = state.running.get(key) {
            return Admission::Join(running.subscribe());
        }
        for (scope, name, limit) in [("device", device, 6), ("provider", provider, 12)] {
            let window = state
                .windows
                .entry((scope.to_owned(), name.to_owned()))
                .or_default();
            while window
                .front()
                .is_some_and(|at| now.duration_since(*at) >= WINDOW)
            {
                window.pop_front();
            }
            if window.len() >= limit {
                let wait = WINDOW
                    .saturating_sub(now.duration_since(*window.front().expect("full window")));
                return Admission::Limited(
                    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1),
                    scope,
                );
            }
        }
        for (scope, name) in [("device", device), ("provider", provider)] {
            state
                .windows
                .entry((scope.to_owned(), name.to_owned()))
                .or_default()
                .push_back(now);
        }
        let (sender, receiver) = watch::channel(None);
        state.running.insert(key.to_owned(), sender);
        Admission::Start(receiver)
    }

    pub(super) fn complete(&self, key: &str, result: Value) {
        let mut state = self.inner.lock().expect("check budget lock");
        if result["ok"] == true {
            state
                .passed
                .insert(key.to_owned(), (Instant::now(), result.clone()));
        }
        if let Some(sender) = state.running.remove(key) {
            let _ = sender.send(Some(result));
        }
    }
}

/// The identity of one check; the credential enters only as a short digest, so a new key checks anew.
pub(super) fn check_key(
    task: &str,
    stage: &SpeechStage,
    language: Option<&str>,
    credential: &str,
) -> String {
    let digest = Sha256::digest(credential.as_bytes());
    let revision = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    json!([
        task,
        stage.place,
        stage.model,
        stage.options,
        language,
        revision
    ])
    .to_string()
}
