//! The node's signing identity and the nonces it signs.

use std::io;

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use rand::rngs::OsRng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::now;
use crate::storage::PrivateDir;

fn invalid(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
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

/// Whether `nonce` is base64url for 16 to 64 bytes, the challenge `NodeIdentity::sign` accepts.
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
