//! Paired connectors: their persisted records, credential checks and one-time pairing codes.
use std::collections::HashMap;

use base64::Engine;
use rand::RngCore;
use serde_json::{json, Map, Value};

use super::util::{field, hash, id, seconds};

const CODE_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_TTL: u64 = 180;

#[derive(Default)]
pub(super) struct Credentials {
    connectors: Map<String, Value>,
    pairing: HashMap<String, u64>,
}
impl Credentials {
    pub(super) fn from_state(state: &Value) -> Self {
        Self {
            connectors: state
                .get("connectors")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            pairing: HashMap::new(),
        }
    }
    /// What the room persists: the connector records, never the pairing codes.
    pub(super) fn state(&self) -> Value {
        json!({"connectors": self.connectors})
    }
    pub(super) fn get(&self, cid: &str) -> Option<&Value> {
        self.connectors.get(cid)
    }
    /// Whether `token` is the current, unrevoked credential of connector `cid`.
    pub(super) fn is_paired(&self, cid: &str, token: &str) -> bool {
        if cid.is_empty() || token.is_empty() {
            return false;
        }
        let Some(entry) = self.connectors.get(cid) else {
            return false;
        };
        let expected = field(entry, "token_hash");
        let supplied = hash(token);
        // Compare in constant time: a timing difference would leak the stored hash.
        if expected.len() != supplied.len()
            || expected
                .as_bytes()
                .iter()
                .zip(supplied.as_bytes())
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                != 0
        {
            return false;
        }
        !entry
            .get("revoked")
            .is_some_and(|v| v == true || v.as_i64().unwrap_or_default() != 0)
    }
    /// Record a new connector with a fresh token, copying what `identity` says about its machine.
    /// Returns the connector's ID and its token; only the token's hash is kept.
    pub(super) fn issue(&mut self, identity: Option<&Value>) -> (String, String) {
        let cid = id();
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let at = seconds();
        let mut record = Map::new();
        record.insert("token_hash".into(), json!(hash(&token)));
        record.insert("created".into(), json!(at));
        record.insert("last_seen".into(), json!(at));
        record.insert("revoked".into(), json!(0));
        if let Some(identity) = identity {
            for key in ["host", "platform", "version", "harnesses"] {
                if let Some(value) = identity.get(key) {
                    record.insert(key.into(), value.clone());
                }
            }
        }
        self.connectors.insert(cid.clone(), Value::Object(record));
        (cid, token)
    }
    /// Note that `cid` was just seen, refreshing what it reports about its machine.
    pub(super) fn touch(&mut self, cid: &str, identity: &Value) {
        let Some(entry) = self.connectors.get_mut(cid).and_then(Value::as_object_mut) else {
            return;
        };
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
    /// A new one-time pairing code, valid for a few minutes, without separators.
    pub(super) fn new_pairing_code(&mut self) -> String {
        let mut bytes = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let code: String = bytes
            .iter()
            .map(|b| CODE_ALPHABET[(*b as usize) % CODE_ALPHABET.len()] as char)
            .collect();
        self.pairing.retain(|_, expiry| *expiry >= seconds());
        self.pairing.insert(code.clone(), seconds() + CODE_TTL);
        code
    }
    /// Consume a pairing code as a person typed it; true if it was issued and is still valid.
    pub(super) fn redeem_pairing_code(&mut self, typed: &str) -> bool {
        let normalized: String = typed
            .chars()
            .filter(|c| !" -_.".contains(*c))
            .map(|c| match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            })
            .collect();
        self.pairing
            .remove(&normalized)
            .is_some_and(|expiry| expiry >= seconds())
    }
    /// Every paired connector without its token hash, oldest first.
    pub(super) fn views(&self, connected: impl Fn(&str) -> bool) -> Value {
        let mut entries: Vec<Value> = self
            .connectors
            .iter()
            .map(|(id, row)| {
                let mut result = row.as_object().cloned().unwrap_or_default();
                result.remove("token_hash");
                result.insert("id".into(), json!(id));
                result.insert("connected".into(), json!(connected(id)));
                Value::Object(result)
            })
            .collect();
        entries.sort_by_key(|e| e.get("created").and_then(Value::as_u64).unwrap_or_default());
        json!(entries)
    }
}
