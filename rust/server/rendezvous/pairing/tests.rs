use super::*;

#[test]
fn pairing_and_public_origin_keep_existing_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    fs::write(&path, r#"{"url":"wss://room.example:444/link","connector_id":"node-1","token":"secret","dial_key":"proof"}"#).unwrap();
    let pairing = Pairing::read(&path).unwrap();
    assert_eq!(pairing.origin, "https://room.example:444");
    assert_eq!(pairing.dial_key.as_deref(), Some("proof"));
    assert_eq!(
        pairing.room_for_devices(None),
        serde_json::json!({"url":"https://room.example:444","node":"node-1"})
    );
    assert_eq!(
        public_origin(" https://room.example/ "),
        Some("https://room.example".into())
    );
    fs::write(
        &path,
        r#"{"url":"file:///tmp/key","connector_id":"node-1","token":"secret"}"#,
    )
    .unwrap();
    assert!(Pairing::read(&path).is_none());
    fs::write(
        &path,
        r#"{"url":"ws://[::1]:8768/link","connector_id":"node-1","token":"secret"}"#,
    )
    .unwrap();
    assert_eq!(Pairing::read(&path).unwrap().origin, "http://[::1]:8768");
}
