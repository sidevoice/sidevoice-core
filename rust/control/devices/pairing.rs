//! One-time pairing secrets, held in memory only as hashes until redeemed or expired.

use std::collections::VecDeque;

use super::{digest, now, secret};

pub(super) const PAIRING_TTL_SECONDS: i64 = 600;
const MAX_PENDING: usize = 5;

#[derive(Default)]
pub(super) struct PairingSecrets {
    pending: VecDeque<(String, i64)>,
}

impl PairingSecrets {
    /// Issue a fresh secret and its expiry, dropping expired ones and keeping the newest few.
    pub(super) fn issue(&mut self) -> (String, i64) {
        let at = now();
        self.pending.retain(|(_, expires)| *expires >= at);
        let value = secret(16);
        let expires = at + PAIRING_TTL_SECONDS;
        self.pending.push_back((digest(&value), expires));
        while self.pending.len() > MAX_PENDING {
            self.pending.pop_front();
        }
        (value, expires)
    }

    /// Consume `value` if it is pending; it pairs only if it has not expired.
    pub(super) fn redeem(&mut self, value: &str) -> bool {
        let hashed = digest(value);
        let Some(index) = self.pending.iter().position(|(hash, _)| hash == &hashed) else {
            return false;
        };
        let (_, expires) = self.pending.remove(index).expect("position exists");
        expires >= now()
    }
}
