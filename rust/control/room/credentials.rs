//! Connector credentials and pairing: the persisted connector records and one-time pairing codes.
use std::io;

use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};

use super::util::{field, hash, id, seconds};
use super::{Inner, Room};

impl Room {
    pub fn local_credential(&self) -> io::Result<(String, String)> {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(saved) = self
            .dir
            .read_json("connector-credential.json")
            .ok()
            .flatten()
        {
            let cid = field(&saved, "connector_id");
            let token = field(&saved, "token");
            if self.credential_locked(&inner, cid, token) == "paired" {
                return Ok((cid.into(), token.into()));
            }
        }
        let cid = id();
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let at = seconds();
        inner.connectors.insert(
            cid.clone(),
            json!({"token_hash": hash(&token), "created": at, "last_seen": at, "revoked": 0}),
        );
        self.save(&inner)?;
        self.dir.write_json(
            "connector-credential.json",
            &json!({"connector_id": cid, "token": token}),
        )?;
        Ok((cid, token))
    }
    fn credential_locked(&self, inner: &Inner, cid: &str, token: &str) -> &'static str {
        if cid.is_empty() || token.is_empty() {
            return "unknown";
        }
        let Some(entry) = inner.connectors.get(cid) else {
            return "unknown";
        };
        let expected = field(entry, "token_hash");
        let supplied = hash(token);
        if expected.len() != supplied.len()
            || expected
                .as_bytes()
                .iter()
                .zip(supplied.as_bytes())
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                != 0
        {
            return "unknown";
        }
        if entry
            .get("revoked")
            .is_some_and(|v| v == true || v.as_i64().unwrap_or_default() != 0)
        {
            "revoked"
        } else {
            "paired"
        }
    }
    pub fn authenticate_connector(&self, cid: &str, token: &str, identity: &Value) -> bool {
        let mut inner = self.inner.lock().expect("room lock");
        if self.credential_locked(&inner, cid, token) != "paired" {
            return false;
        }
        if let Some(entry) = inner.connectors.get_mut(cid).and_then(Value::as_object_mut) {
            entry.insert("last_seen".into(), json!(seconds()));
            for (key, limit) in [("host", 200), ("platform", 60), ("version", 40)] {
                if let Some(s) = identity
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    entry.insert(
                        key.into(),
                        json!(s.trim().chars().take(limit).collect::<String>()),
                    );
                }
            }
            if let Some(names) = identity.get("harnesses").and_then(Value::as_array) {
                entry.insert(
                    "harnesses".into(),
                    json!(names
                        .iter()
                        .take(8)
                        .filter_map(Value::as_str)
                        .map(|s| s.trim().chars().take(40).collect::<String>())
                        .collect::<Vec<_>>()),
                );
            }
        }
        self.save(&inner).is_ok()
    }
    pub fn pairing_code(&self) -> String {
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut bytes = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let code: String = bytes
            .iter()
            .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
            .collect();
        let mut inner = self.inner.lock().expect("room lock");
        inner.pairing.retain(|_, expiry| *expiry >= seconds());
        inner.pairing.insert(code.clone(), seconds() + 180);
        format!("{}-{}-{}", &code[..4], &code[4..8], &code[8..])
    }
    pub fn redeem_pairing(
        &self,
        code: &str,
        identity: &Value,
    ) -> io::Result<Option<(String, String)>> {
        let normalized: String = code
            .chars()
            .filter(|c| !" -_.".contains(*c))
            .map(|c| match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            })
            .collect();
        let mut inner = self.inner.lock().expect("room lock");
        if inner
            .pairing
            .remove(&normalized)
            .is_none_or(|expiry| expiry < seconds())
        {
            return Ok(None);
        }
        let cid = id();
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut record = json!({"token_hash": hash(&token), "created": seconds(), "last_seen": seconds(), "revoked": 0});
        if let Some(obj) = record.as_object_mut() {
            for key in ["host", "platform", "version", "harnesses"] {
                if let Some(value) = identity.get(key) {
                    obj.insert(key.into(), value.clone());
                }
            }
        }
        inner.connectors.insert(cid.clone(), record);
        self.save(&inner)?;
        Ok(Some((cid, token)))
    }
    pub fn paired_connectors(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let mut entries: Vec<Value> = inner
            .connectors
            .iter()
            .map(|(id, row)| {
                let mut result = row.as_object().cloned().unwrap_or_default();
                result.remove("token_hash");
                result.insert("id".into(), json!(id));
                result.insert("connected".into(), json!(inner.peers.contains_key(id)));
                Value::Object(result)
            })
            .collect();
        entries.sort_by_key(|e| e.get("created").and_then(Value::as_u64).unwrap_or_default());
        json!(entries)
    }
}
