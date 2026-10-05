//! One transcription trial at a time per device, and at most six a minute.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LIMIT_WINDOW: Duration = Duration::from_secs(60);
const LIMIT: usize = 6;

#[derive(Default, Clone)]
pub(in crate::server) struct TrialBudget {
    inner: Arc<Mutex<HashMap<String, TrialWindow>>>,
}

#[derive(Default)]
struct TrialWindow {
    active: bool,
    starts: VecDeque<Instant>,
}

/// The device's running trial; dropping it lets the device start another.
pub(super) struct TrialLease {
    budget: TrialBudget,
    device: String,
}

impl Drop for TrialLease {
    fn drop(&mut self) {
        if let Some(window) = self
            .budget
            .inner
            .lock()
            .expect("trial budget lock")
            .get_mut(&self.device)
        {
            window.active = false;
        }
    }
}

impl TrialBudget {
    /// A lease for a new trial, or the seconds to wait before one may start.
    pub(super) fn start(&self, device: &str) -> Result<TrialLease, u64> {
        let now = Instant::now();
        let mut windows = self.inner.lock().expect("trial budget lock");
        windows.retain(|_, window| {
            window.active
                || window
                    .starts
                    .back()
                    .is_some_and(|at| now.duration_since(*at) < LIMIT_WINDOW)
        });
        let window = windows.entry(device.to_owned()).or_default();
        while window
            .starts
            .front()
            .is_some_and(|at| now.duration_since(*at) >= LIMIT_WINDOW)
        {
            window.starts.pop_front();
        }
        if window.active {
            return Err(1);
        }
        if window.starts.len() >= LIMIT {
            let wait = LIMIT_WINDOW
                .saturating_sub(now.duration_since(*window.starts.front().expect("full window")));
            return Err((wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1));
        }
        window.active = true;
        window.starts.push_back(now);
        Ok(TrialLease {
            budget: self.clone(),
            device: device.to_owned(),
        })
    }
}
