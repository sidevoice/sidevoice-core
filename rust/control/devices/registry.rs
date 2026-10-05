//! Paired devices, persisted as token hashes, and the pairing that enrols them.

use std::io;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::pairing::{PairingSecrets, PAIRING_TTL_SECONDS};
use super::{digest, now, secret, NodeIdentity};
use crate::storage::PrivateDir;

const DEVICES_FILE: &str = "devices.json";

pub struct DeviceRegistry {
    dir: PrivateDir,
    devices: Map<String, Value>,
    secrets: PairingSecrets,
}

impl DeviceRegistry {
    /// Load the saved devices, ignoring an unreadable file and any row without a valid token hash.
    pub fn load(dir: PrivateDir) -> io::Result<Self> {
        let devices = dir
            .read_json(DEVICES_FILE)
            .ok()
            .flatten()
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
        let payload = json!({"v": 1, "fp": identity.fingerprint, "host": host, "urls": urls,
            "rv": null, "secret": secret, "exp": expires});
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("JSON object"));
        json!({"code": format!("SV1.{encoded}"), "payload": payload, "expires_in": PAIRING_TTL_SECONDS})
    }

    /// Enrol a device for a pending, unexpired pairing secret; `None` if it is not one.
    pub fn redeem(
        &mut self,
        value: &str,
        name: Option<&str>,
    ) -> io::Result<Option<(String, String)>> {
        if !self.secrets.redeem(value) {
            return Ok(None);
        }
        self.enrol(name, "code", &[]).map(Some)
    }

    /// Enrol the local device, replacing any previous one; returns the replaced ids too.
    pub fn pair_local(&mut self, name: Option<&str>) -> io::Result<(String, String, Vec<String>)> {
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
            let _ = self.save(next);
        }
        Some(id)
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

    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        if !self.devices.contains_key(id) {
            return Ok(false);
        }
        self.save(self.without(&[id.to_owned()]))?;
        Ok(true)
    }

    pub fn revoke_local(&mut self) -> io::Result<Vec<String>> {
        let removed = self.local_ids();
        if !removed.is_empty() {
            self.save(self.without(&removed))?;
        }
        Ok(removed)
    }

    fn enrol(
        &mut self,
        name: Option<&str>,
        kind: &str,
        removed: &[String],
    ) -> io::Result<(String, String)> {
        let id = Uuid::new_v4().to_string();
        let token = secret(32);
        let at = now();
        let mut next = self.without(removed);
        next.insert(
            id.clone(),
            json!({"token_hash": digest(&token), "name": clean_name(name),
            "kind": kind, "created": at, "last_seen": at}),
        );
        self.save(next)?;
        Ok((id, token))
    }

    fn local_ids(&self) -> Vec<String> {
        self.devices
            .iter()
            .filter(|(_, row)| row.get("kind").and_then(Value::as_str) == Some("local"))
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn without(&self, ids: &[String]) -> Map<String, Value> {
        let mut next = self.devices.clone();
        for id in ids {
            next.remove(id);
        }
        next
    }

    /// Persist `next` first; memory changes only once the file is written.
    fn save(&mut self, next: Map<String, Value>) -> io::Result<()> {
        self.dir
            .write_json(DEVICES_FILE, &json!({"devices": next}))?;
        self.devices = next;
        Ok(())
    }
}

/// Collapse whitespace and keep at most 100 characters; an empty name is no name.
fn clean_name(name: Option<&str>) -> Option<String> {
    name.map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(100).collect::<String>())
}
