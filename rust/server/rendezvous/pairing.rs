//! The connector-owned pairing file, and the room origins derived from it.

use std::fs;
use std::path::Path;

use serde_json::Value;
use url::Url;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pairing {
    pub(super) url: String,
    pub(super) origin: String,
    pub(super) connector_id: String,
    pub(super) token: String,
    pub(super) dial_key: Option<String>,
}

impl Pairing {
    /// Read only the connector-owned file. A missing or malformed pairing is
    /// indistinguishable from an unpaired machine to the watcher.
    pub(super) fn read(path: &Path) -> Option<Self> {
        let saved: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let field = |name| {
            saved
                .get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let url = field("url")?.to_owned();
        let origin = http_origin(&url)?;
        Some(Self {
            url,
            origin,
            connector_id: field("connector_id")?.to_owned(),
            token: field("token")?.to_owned(),
            dial_key: field("dial_key").map(str::to_owned),
        })
    }

    pub(super) fn room_for_devices(&self, public_url: Option<&str>) -> Value {
        serde_json::json!({"url": public_url.unwrap_or(&self.origin), "node": self.connector_id})
    }
}

/// The HTTP(S) origin of a WebSocket or HTTP pairing URL.
fn http_origin(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let scheme = match parsed.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return None,
    };
    let authority_host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let mut origin = format!("{scheme}://{authority_host}");
    if let Some(port) = parsed.port() {
        origin.push_str(&format!(":{port}"));
    }
    Some(origin)
}

pub(super) fn public_origin(value: &str) -> Option<String> {
    if value.len() > 2048 {
        return None;
    }
    let trimmed = value.trim().trim_end_matches('/');
    let url = Url::parse(trimmed).ok()?;
    matches!(url.scheme(), "http" | "https")
        .then(|| url.host_str())
        .flatten()
        .map(|_| trimmed.to_owned())
}
