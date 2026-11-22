//! Paired devices, persisted as token hashes, and the pairing that enrols them.

use std::io;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::pairing::{PairingSecrets, PAIRING_TTL_SECONDS};
use super::{digest, now, secret, NodeIdentity};
use crate::storage::PrivateDir;

const DEVICES_FILE: &str = "devices.json";
/// How many removed devices are remembered, oldest forgotten first.
const TOMBSTONES: usize = 64;

pub struct DeviceRegistry {
    dir: PrivateDir,
    devices: Map<String, Value>,
    /// Token hashes of removed devices, oldest first: what tells a removed device from one never paired.
    removed: Vec<String>,
    secrets: PairingSecrets,
}

impl DeviceRegistry {
    /// Load the saved devices, ignoring a file that does not parse and any row without a valid token hash; one that
    /// cannot be trusted (a link, open to others) is refused, never read or replaced.
    pub fn load(dir: PrivateDir) -> io::Result<Self> {
        let saved = match dir.read_json(DEVICES_FILE) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
            read => read?,
        };
        let removed = saved
            .as_ref()
            .and_then(|v| v.get("removed").and_then(Value::as_array))
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .filter(|hash| hash.len() == 64)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let devices = saved
            .and_then(|v| v.get("devices").and_then(Value::as_object).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, entry)| {
                entry
                    .get("token_hash")
                    .and_then(Value::as_str)
                    .is_some_and(|hash| hash.len() == 64)
            })
            .collect();
        Ok(Self {
            dir,
            devices,
            removed,
            secrets: PairingSecrets::default(),
        })
    }

    pub fn issue_code(
        &mut self,
        identity: &NodeIdentity,
        host: Option<&str>,
        urls: &[String],
    ) -> Value {
        let (secret, expires) = self.secrets.issue();
        // The pairing code is the one thing this answer exists to hand over.
        #[allow(
            clippy::disallowed_methods,
            reason = "the pairing code is sent to the person who asked"
        )]
        let payload = json!({"v": 1, "fp": identity.fingerprint, "host": host, "urls": urls,
            "secret": secret.expose_secret(), "exp": expires});
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("JSON object"));
        json!({"code": format!("SV1.{encoded}"), "payload": payload, "expires_in": PAIRING_TTL_SECONDS})
    }

    /// Enrol a device for a pending, unexpired pairing secret; `None` if it is not one.
    pub fn redeem(
        &mut self,
        value: &SecretString,
        name: Option<&str>,
    ) -> io::Result<Option<(String, SecretString)>> {
        if !self.secrets.redeem(value) {
            return Ok(None);
        }
        self.enrol(name, "code", &[]).map(Some)
    }

    /// Enrol the local device, replacing any previous one; returns the replaced ids too.
    pub fn pair_local(
        &mut self,
        name: Option<&str>,
    ) -> io::Result<(String, SecretString, Vec<String>)> {
        let removed = self.local_ids();
        let (id, token) = self.enrol(name, "local", &removed)?;
        Ok((id, token, removed))
    }

    /// The device id for `token`, refreshing its `last_seen` at most once a minute.
    pub fn authenticate(&mut self, token: &str) -> Option<String> {
        let hashed = digest(token);
        let id = self
            .devices
            .iter()
            .find(|(_, row)| {
                row.get("token_hash").and_then(Value::as_str) == Some(hashed.as_str())
            })?
            .0
            .clone();
        let at = now();
        if self
            .devices
            .get(&id)?
            .get("last_seen")
            .and_then(Value::as_i64)
            .unwrap_or_default()
            <= at - 60
        {
            let mut next = self.devices.clone();
            next.get_mut(&id)?
                .as_object_mut()?
                .insert("last_seen".to_owned(), json!(at));
            let _ = self.save(next, self.removed.clone());
        }
        Some(id)
    }

    /// Whether `token` belonged to a device that was removed (and not merely never paired).
    pub fn was_removed(&self, token: &str) -> bool {
        self.removed.contains(&digest(token))
    }

    pub fn listing(&self, current: &str) -> Value {
        let mut rows: Vec<_> = self.devices.iter().map(|(id, row)| json!({
            "id": id, "name": row.get("name"), "kind": row.get("kind").and_then(Value::as_str).unwrap_or("code"),
            "created": row.get("created"), "last_seen": row.get("last_seen"), "current": id == current
        })).collect();
        rows.sort_by_key(|row| {
            row.get("created")
                .and_then(Value::as_i64)
                .unwrap_or_default()
        });
        json!({"devices": rows})
    }

    /// The ids of the devices seen within the last `seconds`.
    pub fn seen_within(&self, seconds: i64) -> std::collections::HashSet<String> {
        let since = now() - seconds;
        self.devices
            .iter()
            .filter(|(_, row)| {
                row.get("last_seen")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    >= since
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Whether device `id` is still paired.
    pub fn contains(&self, id: &str) -> bool {
        self.devices.contains_key(id)
    }

    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        if !self.devices.contains_key(id) {
            return Ok(false);
        }
        self.remove(&[id.to_owned()])?;
        Ok(true)
    }

    /// Give device `id` a new name, already validated; false if there is no such device.
    pub fn rename(&mut self, id: &str, name: &str) -> io::Result<bool> {
        let mut next = self.devices.clone();
        let Some(row) = next.get_mut(id).and_then(Value::as_object_mut) else {
            return Ok(false);
        };
        row.insert("name".to_owned(), json!(name));
        self.save(next, self.removed.clone())?;
        Ok(true)
    }

    pub fn revoke_local(&mut self) -> io::Result<Vec<String>> {
        let removed = self.local_ids();
        if !removed.is_empty() {
            self.remove(&removed)?;
        }
        Ok(removed)
    }

    fn enrol(
        &mut self,
        name: Option<&str>,
        kind: &str,
        removed: &[String],
    ) -> io::Result<(String, SecretString)> {
        let id = Uuid::new_v4().to_string();
        let token = SecretString::from(secret(32));
        let at = now();
        let (mut next, tombstones) = self.without(removed);
        #[allow(
            clippy::disallowed_methods,
            reason = "only the hash of the token is stored"
        )]
        next.insert(
            id.clone(),
            json!({"token_hash": digest(token.expose_secret()), "name": clean_name(name),
            "kind": kind, "created": at, "last_seen": at}),
        );
        self.save(next, tombstones)?;
        Ok((id, token))
    }

    fn local_ids(&self) -> Vec<String> {
        self.devices
            .iter()
            .filter(|(_, row)| row.get("kind").and_then(Value::as_str) == Some("local"))
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn remove(&mut self, ids: &[String]) -> io::Result<()> {
        let (next, tombstones) = self.without(ids);
        self.save(next, tombstones)
    }

    /// The devices left once `ids` go, and the tombstones that then stand (bounded, oldest dropped).
    fn without(&self, ids: &[String]) -> (Map<String, Value>, Vec<String>) {
        let mut next = self.devices.clone();
        let mut tombstones = self.removed.clone();
        for id in ids {
            if let Some(row) = next.remove(id) {
                if let Some(hash) = row.get("token_hash").and_then(Value::as_str) {
                    tombstones.push(hash.to_owned());
                }
            }
        }
        let extra = tombstones.len().saturating_sub(TOMBSTONES);
        tombstones.drain(..extra);
        (next, tombstones)
    }

    /// Persist first; memory changes only once the file is written.
    fn save(&mut self, next: Map<String, Value>, removed: Vec<String>) -> io::Result<()> {
        self.dir
            .write_json(DEVICES_FILE, &json!({"devices": next, "removed": removed}))?;
        self.devices = next;
        self.removed = removed;
        Ok(())
    }
}

/// Collapse whitespace and keep at most 100 characters; an empty name is no name.
fn clean_name(name: Option<&str>) -> Option<String> {
    name.map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(100).collect::<String>())
}
