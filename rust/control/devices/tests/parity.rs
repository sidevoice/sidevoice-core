//! Node identity, pairing codes and device tokens, ported from the Python suite
//! (`tests/test_device_pairing.py` `StoreTests` and the store half of `NodeSurfaceTests`).
//! Pairing-secret expiry is `PairingSecrets`' own, covered beside it.

use std::os::unix::fs::PermissionsExt;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use p256::pkcs8::DecodePublicKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::super::{digest, now, valid_nonce, DeviceRegistry, NodeIdentity};
use crate::storage::PrivateDir;

fn private_dir() -> (tempfile::TempDir, PrivateDir) {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    (root, dir)
}

fn names(dir: &PrivateDir) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn issue(registry: &mut DeviceRegistry, identity: &NodeIdentity) -> String {
    registry.issue_code(identity, None, &[])["payload"]["secret"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn decode_code(code: &str) -> Value {
    let encoded = code.strip_prefix("SV1.").expect("SV1. prefix");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap()
}

#[test]
fn the_identity_is_created_once_kept_private_and_stable() {
    let (_root, dir) = private_dir();
    let first = NodeIdentity::load_or_create(&dir).unwrap();
    let path = dir.path().join("node-identity.json");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let saved = dir.read_json("node-identity.json").unwrap().unwrap();
    let mut keys: Vec<_> = saved.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["created", "private_key_pem"]);
    let again = NodeIdentity::load_or_create(&dir).unwrap();
    assert_eq!(
        (&again.public_key, &again.fingerprint),
        (&first.public_key, &first.fingerprint),
        "the key survives a restart"
    );
    let der = STANDARD.decode(&first.public_key).unwrap();
    VerifyingKey::from_public_key_der(&der).expect("a P-256 public key");
    assert_eq!(
        first.fingerprint,
        URL_SAFE_NO_PAD.encode(Sha256::digest(&der))
    );
    assert_eq!(first.fingerprint.len(), 43);
    assert_eq!(
        names(&dir),
        ["node-identity.json"],
        "no staging file is left behind"
    );
}

#[test]
fn an_identity_that_cannot_be_read_is_refused_never_replaced() {
    for content in [
        &b"{\"private_key_pem\": \"not a key\"}"[..],
        b"not even json",
        b"\xff\xfe",
        b"{\"private_key_pem\": \"\xc3\x28\"}",
    ] {
        let (_root, dir) = private_dir();
        std::fs::write(dir.path().join("node-identity.json"), content).unwrap();
        assert!(NodeIdentity::load_or_create(&dir).is_err(), "{content:?}");
        assert_eq!(
            std::fs::read(dir.path().join("node-identity.json")).unwrap(),
            content
        );
    }
}

#[test]
fn the_identity_proof_verifies_with_the_pinned_key() {
    let (_root, dir) = private_dir();
    let identity = NodeIdentity::load_or_create(&dir).unwrap();
    let key =
        VerifyingKey::from_public_key_der(&STANDARD.decode(&identity.public_key).unwrap()).unwrap();
    let nonce = URL_SAFE_NO_PAD.encode([7u8; 32]);
    let raw = URL_SAFE_NO_PAD.decode(identity.sign(&nonce)).unwrap();
    assert_eq!(raw.len(), 64, "P1363 r||s");
    let signature = Signature::from_slice(&raw).unwrap();
    key.verify(
        format!("sidevoice-node-identity:{nonce}").as_bytes(),
        &signature,
    )
    .unwrap();
    let other = URL_SAFE_NO_PAD.encode([8u8; 32]);
    assert!(key
        .verify(
            format!("sidevoice-node-identity:{other}").as_bytes(),
            &signature
        )
        .is_err());
}

#[test]
fn a_nonce_is_url_safe_base64_of_sixteen_to_sixty_four_bytes() {
    assert!(valid_nonce(&URL_SAFE_NO_PAD.encode([1u8; 16])));
    assert!(valid_nonce(&URL_SAFE_NO_PAD.encode([1u8; 64])));
    assert!(valid_nonce(
        &base64::engine::general_purpose::URL_SAFE.encode([1u8; 16])
    ));
    for wrong in [
        String::new(),
        URL_SAFE_NO_PAD.encode([1u8; 15]),
        URL_SAFE_NO_PAD.encode([1u8; 65]),
        "not base64url!!!!!!!!!!!!!".to_owned(),
    ] {
        assert!(!valid_nonce(&wrong), "{wrong}");
    }
}

#[test]
fn a_code_is_the_payload_it_carries() {
    let (_root, dir) = private_dir();
    let identity = NodeIdentity::load_or_create(&dir).unwrap();
    let mut registry = DeviceRegistry::load(dir.clone()).unwrap();
    let urls = vec!["http://127.0.0.1:8768".to_owned()];
    let issued = registry.issue_code(&identity, Some("laptop"), &urls);
    let code = issued["code"].as_str().unwrap();
    assert!(code.starts_with("SV1."));
    assert!(!code.contains('='));
    assert_eq!(issued["expires_in"], 600);
    let payload = decode_code(code);
    assert_eq!(payload, issued["payload"]);
    let mut keys: Vec<_> = payload.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["exp", "fp", "host", "rv", "secret", "urls", "v"]);
    assert_eq!(payload["v"], 1);
    assert_eq!(payload["fp"], identity.fingerprint.as_str());
    assert_eq!(payload["host"], "laptop");
    assert_eq!(payload["urls"], json!(urls));
    assert_eq!(
        URL_SAFE_NO_PAD
            .decode(payload["secret"].as_str().unwrap())
            .unwrap()
            .len(),
        16
    );
    assert!((payload["exp"].as_i64().unwrap() - (now() + 600)).abs() <= 5);
    let bare = registry.issue_code(&identity, None, &[])["payload"].clone();
    assert_eq!(
        (&bare["host"], &bare["rv"], &bare["urls"]),
        (&Value::Null, &Value::Null, &json!([])),
        "absent values are null"
    );
}

#[test]
fn only_a_handful_of_codes_are_outstanding_and_each_is_one_time() {
    let (_root, dir) = private_dir();
    let identity = NodeIdentity::load_or_create(&dir).unwrap();
    let mut registry = DeviceRegistry::load(dir).unwrap();
    let issued: Vec<String> = (0..6).map(|_| issue(&mut registry, &identity)).collect();
    assert!(
        registry.redeem(&issued[0], Some("x")).unwrap().is_none(),
        "the oldest was dropped"
    );
    assert!(registry.redeem(&issued[5], Some("x")).unwrap().is_some());
    assert!(
        registry.redeem(&issued[5], Some("x")).unwrap().is_none(),
        "one-time"
    );
    assert!(registry.redeem("guessed", Some("x")).unwrap().is_none());
}

#[test]
fn a_code_is_redeemed_once_and_only_its_hash_is_kept() {
    let (_root, dir) = private_dir();
    let identity = NodeIdentity::load_or_create(&dir).unwrap();
    let mut registry = DeviceRegistry::load(dir.clone()).unwrap();
    let secret = issue(&mut registry, &identity);
    let (device_id, token) = registry
        .redeem(&secret, Some("  Mi   portátil "))
        .unwrap()
        .unwrap();
    Uuid::parse_str(&device_id).unwrap();
    assert_eq!(URL_SAFE_NO_PAD.decode(&token).unwrap().len(), 32);
    let path = dir.path().join("devices.json");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains(&token), "the token itself is never written");
    let entry = &dir.read_json("devices.json").unwrap().unwrap()["devices"][&device_id];
    assert_eq!(entry["token_hash"], digest(&token));
    assert_eq!(entry["name"], "Mi portátil");
    assert_eq!(entry["kind"], "code");
    let reloaded = DeviceRegistry::load(dir).unwrap().authenticate(&token);
    assert_eq!(
        reloaded.as_deref(),
        Some(device_id.as_str()),
        "kept across a restart"
    );
}

/// A saved device whose token is `token`, last seen at `last_seen`.
fn saved_device(dir: &PrivateDir, token: &str, last_seen: i64) {
    dir.write_json(
        "devices.json",
        &json!({"devices": {"phone": {"token_hash": digest(token), "name": "phone",
            "kind": "code", "created": last_seen, "last_seen": last_seen}}}),
    )
    .unwrap();
}

#[test]
fn last_seen_moves_at_most_once_a_minute() {
    let (_root, dir) = private_dir();
    let recent = now() - 30;
    saved_device(&dir, "phone-token", recent);
    let written = std::fs::read(dir.path().join("devices.json")).unwrap();
    let mut registry = DeviceRegistry::load(dir.clone()).unwrap();
    assert_eq!(
        registry.authenticate("phone-token").as_deref(),
        Some("phone")
    );
    assert_eq!(
        std::fs::read(dir.path().join("devices.json")).unwrap(),
        written,
        "not written inside the minute"
    );
    assert_eq!(registry.listing("phone")["devices"][0]["last_seen"], recent);

    let stale = now() - 61;
    saved_device(&dir, "phone-token", stale);
    let mut registry = DeviceRegistry::load(dir.clone()).unwrap();
    registry.authenticate("phone-token").unwrap();
    let seen = dir.read_json("devices.json").unwrap().unwrap()["devices"]["phone"]["last_seen"]
        .as_i64()
        .unwrap();
    assert!(seen > stale + 60, "moved to now and written");
    assert_eq!(registry.listing("phone")["devices"][0]["last_seen"], seen);
}

#[test]
fn pairing_the_app_again_replaces_only_the_local_device() {
    let (_root, dir) = private_dir();
    let identity = NodeIdentity::load_or_create(&dir).unwrap();
    let mut registry = DeviceRegistry::load(dir.clone()).unwrap();
    let secret = issue(&mut registry, &identity);
    let (phone, phone_token) = registry.redeem(&secret, Some("Móvil")).unwrap().unwrap();
    let (first, first_token, replaced) = registry.pair_local(Some("first")).unwrap();
    assert!(replaced.is_empty());
    let (second, _, replaced) = registry.pair_local(Some("second")).unwrap();
    assert_eq!(replaced, vec![first]);
    assert!(registry.authenticate(&first_token).is_none());
    assert_eq!(
        registry.authenticate(&phone_token).as_deref(),
        Some(phone.as_str())
    );
    let listing = registry.listing(&second)["devices"].clone();
    let mut kinds: Vec<_> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["id"].as_str().unwrap().to_owned(), row["kind"].clone()))
        .collect();
    kinds.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![(phone, json!("code")), (second.clone(), json!("local"))];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        kinds, expected,
        "one local device; a code-paired one is untouched"
    );
    assert_eq!(
        dir.read_json("devices.json").unwrap().unwrap()["devices"][&second]["kind"],
        "local",
        "kept with the device"
    );
    assert_eq!(registry.revoke_local().unwrap(), [second]);
    assert!(
        registry.revoke_local().unwrap().is_empty(),
        "nothing left to revoke"
    );
}

#[test]
fn a_registry_entry_without_a_proper_hash_is_dropped_on_load() {
    let (_root, dir) = private_dir();
    dir.write_json(
        "devices.json",
        &json!({"devices": {"bad": {"token_hash": "short"}, "none": {}}}),
    )
    .unwrap();
    let registry = DeviceRegistry::load(dir).unwrap();
    assert_eq!(registry.listing("")["devices"], json!([]));
}
