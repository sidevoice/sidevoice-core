use super::pairing::PairingSecrets;
use super::DeviceRegistry;
use crate::storage::PrivateDir;

fn registry() -> (tempfile::TempDir, DeviceRegistry) {
    let root = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(root.path().join("core")).unwrap();
    (root, DeviceRegistry::load(dir).unwrap())
}

#[test]
fn a_pairing_secret_redeems_once() {
    let mut secrets = PairingSecrets::default();
    let (secret, _) = secrets.issue();
    assert!(!secrets.redeem("unknown"));
    assert!(secrets.redeem(&secret));
    assert!(!secrets.redeem(&secret));
}

#[test]
fn only_the_newest_pairing_secrets_stay_pending() {
    let mut secrets = PairingSecrets::default();
    let issued: Vec<_> = (0..6).map(|_| secrets.issue().0).collect();
    assert!(!secrets.redeem(&issued[0]));
    assert!(issued[1..].iter().all(|secret| secrets.redeem(secret)));
}

#[test]
fn pairing_the_local_device_replaces_the_previous_one() {
    let (_root, mut registry) = registry();
    let (first, first_token, removed) = registry.pair_local(Some("  Desk \n top ")).unwrap();
    assert!(removed.is_empty());
    let (second, _, removed) = registry.pair_local(None).unwrap();
    assert_eq!(removed, vec![first]);
    assert_eq!(registry.authenticate(&first_token), None);
    assert_eq!(registry.revoke_local().unwrap(), vec![second]);
    assert!(registry.revoke_local().unwrap().is_empty());
}

#[test]
fn devices_persist_and_revoke_by_id() {
    let (root, mut registry) = registry();
    let (id, token, _) = registry.pair_local(Some("  Desk \n top ")).unwrap();
    let reloaded =
        DeviceRegistry::load(PrivateDir::open(root.path().join("core")).unwrap()).unwrap();
    let listing = reloaded.listing(&id);
    assert_eq!(listing["devices"][0]["name"], "Desk top");
    assert_eq!(listing["devices"][0]["kind"], "local");
    assert_eq!(listing["devices"][0]["current"], true);
    assert_eq!(registry.authenticate(&token), Some(id.clone()));
    assert!(registry.revoke(&id).unwrap());
    assert!(!registry.revoke(&id).unwrap());
    assert_eq!(registry.authenticate(&token), None);
}

#[test]
fn a_rename_persists_and_an_unknown_device_is_not_renamed() {
    let (root, mut registry) = registry();
    let (id, _, _) = registry.pair_local(Some("Desk")).unwrap();
    assert!(registry.rename(&id, "Studio").unwrap());
    assert!(!registry.rename("nobody", "Studio").unwrap());
    let reloaded =
        DeviceRegistry::load(PrivateDir::open(root.path().join("core")).unwrap()).unwrap();
    let listing = reloaded.listing(&id);
    assert_eq!(listing["devices"].as_array().unwrap().len(), 1);
    assert_eq!(listing["devices"][0]["name"], "Studio");
    assert_eq!(listing["devices"][0]["kind"], "local");
}

mod parity;
