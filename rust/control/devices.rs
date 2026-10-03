//! The node identity, one-time pairing secrets, and persisted device-token hashes.

use std::collections::VecDeque;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::storage::PrivateDir;

fn invalid(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn secret(bytes: usize) -> String {
    let mut random = vec![0; bytes];
    OsRng.fill_bytes(&mut random);
    URL_SAFE_NO_PAD.encode(random)
}

pub struct NodeIdentity {
    signing: SigningKey,
    pub public_key: String,
    pub fingerprint: String,
}

impl NodeIdentity {
    pub fn load_or_create(dir: &PrivateDir) -> io::Result<Self> {
        if let Some(saved) = dir.read_json("node-identity.json")? {
            return Self::parse(&saved);
        }
        let signing = SigningKey::random(&mut OsRng);
        let pem = signing.to_pkcs8_pem(LineEnding::LF).map_err(invalid)?;
        let bytes = serde_json::to_vec(&json!({"private_key_pem": pem.as_str(), "created": now()}))
            .map_err(invalid)?;
        match dir.link_new("node-identity.json", &bytes) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        let saved = dir
            .read_json("node-identity.json")?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "identity.unreadable"))?;
        Self::parse(&saved)
    }

    fn parse(saved: &Value) -> io::Result<Self> {
        let pem = saved
            .get("private_key_pem")
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "identity.unreadable"))?;
        let signing = SigningKey::from_pkcs8_pem(pem).map_err(invalid)?;
        let public_der = signing
            .verifying_key()
            .to_public_key_der()
            .map_err(invalid)?;
        let public_key = STANDARD.encode(public_der.as_bytes());
        let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(public_der.as_bytes()));
        Ok(Self {
            signing,
            public_key,
            fingerprint,
        })
    }

    pub fn sign(&self, nonce: &str) -> String {
        let payload = format!("sidevoice-node-identity:{nonce}");
        let signature: Signature = self.signing.sign(payload.as_bytes());
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    }

    pub fn public(&self) -> Value {
        json!({"fingerprint": self.fingerprint, "public_key": self.public_key})
    }
}

pub fn valid_nonce(nonce: &str) -> bool {
    if !(22..=86).contains(&nonce.len())
        || !nonce
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'=')
    {
        return false;
    }
    let bare = nonce.trim_end_matches('=');
    if nonce.len() - bare.len() > 2 || bare.len() % 4 == 1 {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(bare)
        .is_ok_and(|bytes| (16..=64).contains(&bytes.len()))
}

pub struct DeviceRegistry {
    dir: PrivateDir,
    devices: Map<String, Value>,
    secrets: VecDeque<(String, i64)>,
}

impl DeviceRegistry {
    pub fn load(dir: PrivateDir) -> io::Result<Self> {
        let devices = dir
            .read_json("devices.json")
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
            secrets: VecDeque::new(),
        })
    }

    pub fn issue_secret(&mut self) -> (String, i64) {
        let at = now();
        self.secrets.retain(|(_, expires)| *expires >= at);
        let value = secret(16);
        let expires = at + 600;
        self.secrets.push_back((digest(&value), expires));
        while self.secrets.len() > 5 {
            self.secrets.pop_front();
        }
        (value, expires)
    }

    pub fn issue_code(
        &mut self,
        identity: &NodeIdentity,
        host: Option<&str>,
        urls: &[String],
    ) -> Value {
        let (secret, expires) = self.issue_secret();
        let payload = json!({"v": 1, "fp": identity.fingerprint, "host": host, "urls": urls,
            "rv": null, "secret": secret, "exp": expires});
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("JSON object"));
        json!({"code": format!("SV1.{encoded}"), "payload": payload, "expires_in": 600})
    }

    pub fn redeem(
        &mut self,
        value: &str,
        name: Option<&str>,
    ) -> io::Result<Option<(String, String)>> {
        let hashed = digest(value);
        let Some(index) = self.secrets.iter().position(|(hash, _)| hash == &hashed) else {
            return Ok(None);
        };
        let (_, expires) = self.secrets.remove(index).expect("position exists");
        if expires < now() {
            return Ok(None);
        }
        self.enrol(name, "code", &[]).map(Some)
    }

    pub fn pair_local(&mut self, name: Option<&str>) -> io::Result<(String, String, Vec<String>)> {
        let removed: Vec<_> = self
            .devices
            .iter()
            .filter(|(_, row)| row.get("kind").and_then(Value::as_str) == Some("local"))
            .map(|(id, _)| id.clone())
            .collect();
        let (id, token) = self.enrol(name, "local", &removed)?;
        Ok((id, token, removed))
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
        let cleaned = name
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty())
            .map(|text| text.chars().take(100).collect::<String>());
        let mut next = self.devices.clone();
        for old in removed {
            next.remove(old);
        }
        next.insert(
            id.clone(),
            json!({"token_hash": digest(&token), "name": cleaned,
            "kind": kind, "created": at, "last_seen": at}),
        );
        self.save(next)?;
        Ok((id, token))
    }

    fn save(&mut self, next: Map<String, Value>) -> io::Result<()> {
        self.dir
            .write_json("devices.json", &json!({"devices": next}))?;
        self.devices = next;
        Ok(())
    }

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
        let mut next = self.devices.clone();
        next.remove(id);
        self.save(next)?;
        Ok(true)
    }

    pub fn revoke_local(&mut self) -> io::Result<Vec<String>> {
        let removed: Vec<_> = self
            .devices
            .iter()
            .filter(|(_, row)| row.get("kind").and_then(Value::as_str) == Some("local"))
            .map(|(id, _)| id.clone())
            .collect();
        if !removed.is_empty() {
            let mut next = self.devices.clone();
            for id in &removed {
                next.remove(id);
            }
            self.save(next)?;
        }
        Ok(removed)
    }
}
