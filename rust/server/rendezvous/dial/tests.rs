use tokio::sync::watch;

use super::*;
use crate::control::room::{ConnectorPeer, Room};
use crate::server::rendezvous::test_support::loopback;
use crate::storage::PrivateDir;

#[test]
fn dial_hello_requires_an_object_without_error() {
    assert!(valid_hello(&serde_json::json!({"protocol": 3})));
    assert!(valid_hello(&serde_json::json!({"error": null})));
    for answer in [
        serde_json::Value::Null,
        serde_json::json!("bad"),
        serde_json::json!([]),
        serde_json::json!({"error":"rejected"}),
    ] {
        assert!(!valid_hello(&answer));
    }
}

#[tokio::test]
async fn malformed_hello_reports_a_connector_visible_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let dir = PrivateDir::open(temp.path().join("core")).unwrap();
    let room = Arc::new(Room::load(dir).unwrap());
    let (sender, mut receiver) = mpsc::channel(2);
    let (stop, _) = watch::channel(false);
    room.attach(
        "fixture",
        ConnectorPeer {
            generation: "one".into(),
            sender,
            stop,
        },
    );
    let rv = Rendezvous::new(None, loopback(), "fixture".into(), room);
    rv.refused(hello_refusal(&serde_json::json!("malformed")))
        .await;
    let event = receiver.recv().await.unwrap();
    assert_eq!(event.method, "node.rendezvous");
    assert_eq!(event.params["connected"], false);
    let reason = event.params["refused"].as_str().unwrap();
    assert!(
        !reason.is_empty(),
        "Connector's truthy refusal branch must run"
    );
    assert_eq!(
        reason,
        render(&LocalizedMessage::new("relay.hello_refused"), "en")
    );
    assert_eq!(rv.snapshot()["refused"].as_str(), Some(reason));
    assert_eq!(
        hello_refusal(&serde_json::json!({"error": "specific"})),
        "specific"
    );
}
