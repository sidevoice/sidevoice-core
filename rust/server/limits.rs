//! Failed-attempt budgets for the network listener. Keys are TCP peer IPs, never forwarded headers.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;

use super::refusal::failure;

pub(super) const PAIR_PER_PEER: u32 = 10;
pub(super) const PAIR_GLOBAL: u32 = 40;
pub(super) const AUTH_PER_PEER: u32 = 60;
pub(super) const AUTH_GLOBAL: u32 = 300;
const WINDOW: Duration = Duration::from_secs(60);

pub(super) struct Limits {
    window: Duration,
    pair: Mutex<Budget>,
    auth: Mutex<Budget>,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new(WINDOW)
    }
}

impl Limits {
    fn new(window: Duration) -> Self {
        Self {
            window,
            pair: Mutex::new(Budget::default()),
            auth: Mutex::new(Budget::default()),
        }
    }

    pub(super) fn check_pair(&self, peer: IpAddr, headers: &HeaderMap) -> Option<Response> {
        self.check(
            &self.pair,
            peer,
            PAIR_PER_PEER,
            PAIR_GLOBAL,
            "pair",
            headers,
        )
    }

    pub(super) fn failed_pair(&self, peer: IpAddr) {
        self.pair
            .lock()
            .expect("pair budget")
            .record(peer, self.window);
    }

    pub(super) fn check_auth(&self, peer: IpAddr, headers: &HeaderMap) -> Option<Response> {
        self.check(
            &self.auth,
            peer,
            AUTH_PER_PEER,
            AUTH_GLOBAL,
            "auth",
            headers,
        )
    }

    pub(super) fn failed_auth(&self, peer: IpAddr) {
        self.auth
            .lock()
            .expect("auth budget")
            .record(peer, self.window);
    }

    fn check(
        &self,
        budget: &Mutex<Budget>,
        peer: IpAddr,
        per_peer: u32,
        global: u32,
        kind: &str,
        headers: &HeaderMap,
    ) -> Option<Response> {
        let mut budget = budget.lock().expect("attempt budget");
        let retry = budget.retry_after(peer, per_peer, global, self.window)?;
        if !budget.logged {
            eprintln!(
                "network {kind} attempt limit reached; retry after {}s",
                retry
            );
            budget.logged = true;
        }
        let mut response = failure(
            "request.rate_limited",
            StatusCode::TOO_MANY_REQUESTS,
            headers,
        );
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&retry.to_string()).expect("retry seconds"),
        );
        Some(response)
    }
}

#[derive(Default)]
struct Budget {
    started: Option<Instant>,
    total: u32,
    peers: HashMap<IpAddr, u32>,
    logged: bool,
}

impl Budget {
    fn refresh(&mut self, window: Duration) {
        if self.started.is_none_or(|start| start.elapsed() >= window) {
            self.started = Some(Instant::now());
            self.total = 0;
            self.peers.clear();
            self.logged = false;
        }
    }

    fn retry_after(
        &mut self,
        peer: IpAddr,
        per_peer: u32,
        global: u32,
        window: Duration,
    ) -> Option<u64> {
        self.refresh(window);
        (self.total >= global
            || self
                .peers
                .get(&peer)
                .is_some_and(|count| *count >= per_peer))
        .then(|| {
            window
                .saturating_sub(self.started.expect("window").elapsed())
                .as_secs()
                .max(1)
        })
    }

    fn record(&mut self, peer: IpAddr, window: Duration) {
        self.refresh(window);
        if self.total < u32::MAX {
            self.total += 1;
            *self.peers.entry(peer).or_default() += 1;
        }
    }
}

#[cfg(test)]
impl Limits {
    pub(super) fn short_window() -> Self {
        Self::new(Duration::from_millis(100))
    }
}
