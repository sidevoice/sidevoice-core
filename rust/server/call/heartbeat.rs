//! The browser link's keepalive: the room asks a quiet browser to say something, and a browser that
//! never answers gives its seat back, as Python's `pipeline/heartbeat.py` does.
//!
//! Behind a tunnel or a proxy a closed tab does not reach the room as a socket close, so its seat
//! would stay taken (2026-09-22). The room asks with `voice-ping`; the page answers `voice-pong`.
//! Anything the browser sends counts as an answer, microphone PCM included, so only a socket that
//! has actually gone quiet is asked.

use std::time::Duration;

/// How often a quiet browser is asked, and how many asks it may miss before it is dropped.
const HEARTBEAT_SECONDS: f64 = 15.0;
const HEARTBEAT_MISSES: f64 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Heartbeat {
    pub interval: Duration,
    budget: Duration,
}

#[derive(Debug, PartialEq)]
pub enum Beat {
    /// Something arrived recently enough: nothing to do.
    Quiet,
    /// Silent for at least one interval: ask with `voice-ping`.
    Ask,
    /// Silent past the whole budget: the browser is gone.
    Drop,
}

impl Heartbeat {
    /// This room's keepalive from `VOICE_BROWSER_HEARTBEAT_SECONDS` and `..._MISSES`, or none when
    /// the interval is zero.
    pub fn from_env() -> Option<Self> {
        Self::configured(
            std::env::var("VOICE_BROWSER_HEARTBEAT_SECONDS")
                .ok()
                .as_deref(),
            std::env::var("VOICE_BROWSER_HEARTBEAT_MISSES")
                .ok()
                .as_deref(),
        )
    }

    /// Python's `heartbeat_settings`: an interval of zero turns the keepalive off; anything
    /// unreadable or negative falls back to the default rather than to no keepalive at all; a budget
    /// of zero misses means the default, since "ask nothing" is said with the interval.
    pub fn configured(seconds: Option<&str>, misses: Option<&str>) -> Option<Self> {
        let interval = number(seconds, HEARTBEAT_SECONDS, 0.01);
        if interval == 0.0 {
            return None;
        }
        let misses = Some(number(misses, HEARTBEAT_MISSES, 1.0))
            .filter(|misses| *misses > 0.0)
            .unwrap_or(HEARTBEAT_MISSES);
        Some(Self {
            interval: Duration::from_secs_f64(interval),
            budget: Duration::from_secs_f64(interval * misses),
        })
    }

    /// What to do after `silence` with nothing at all from the browser.
    pub fn check(&self, silence: Duration) -> Beat {
        if silence >= self.budget {
            Beat::Drop
        } else if silence >= self.interval {
            Beat::Ask
        } else {
            Beat::Quiet
        }
    }
}

fn number(value: Option<&str>, fallback: f64, floor: f64) -> f64 {
    let Some(Ok(value)) = value.map(|value| value.trim().parse::<f64>()) else {
        return fallback;
    };
    if !value.is_finite() || value < 0.0 {
        fallback
    } else if value == 0.0 {
        0.0
    } else {
        value.max(floor)
    }
}

#[cfg(test)]
mod tests;
