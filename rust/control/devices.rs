//! The node identity, one-time pairing secrets, and persisted device-token hashes.

mod identity;
mod pairing;
mod registry;

#[cfg(test)]
mod tests;

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};

pub use identity::{valid_nonce, NodeIdentity};
pub use registry::DeviceRegistry;

/// Seconds since the Unix epoch.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// The lowercase hex SHA-256 under which secrets and tokens are stored.
fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// `bytes` random bytes, base64url-encoded without padding.
fn secret(bytes: usize) -> String {
    let mut random = vec![0; bytes];
    OsRng.fill_bytes(&mut random);
    URL_SAFE_NO_PAD.encode(random)
}
