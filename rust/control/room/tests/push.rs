use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::support::{pull_room, say};
use crate::control::room::bindings::Binding;
use crate::control::room::util::seconds;

#[test]
fn push_falls_back_to_an_idle_older_binding_as_before() {
    let (_directory, room, _) = pull_room(&["connector"]);
    let older = room
        .register("connector", &json!({"thread":"busy","harness":"claude"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    room.inner
        .lock()
        .unwrap()
        .bindings
        .get_mut(&older)
        .unwrap()
        .created -= 10;
    let (events, _received) = mpsc::channel(64);
    let sid = room.join("device".into(), "en".into(), events).unwrap();
    // A second, newer push binding for the same thread from the same connector.
    let newer = {
        let mut inner = room.inner.lock().unwrap();
        let mut binding = Binding {
            id: "newer".into(),
            connector: "connector".into(),
            thread: "busy".into(),
            harness: "claude".into(),
            title: None,
            created: seconds(),
            active: true,
            live: true,
            inbound: None,
            capabilities: Value::Null,
            engine: None,
            route: None,
            pull_input: false,
        };
        binding.created += 1;
        inner.bindings.insert("newer".into(), binding);
        "newer".to_owned()
    };
    room.select(&sid, "busy").unwrap();
    say(&room, &sid, "one");
    say(&room, &sid, "two");
    let delivery = room.pending_delivery();
    let targets: Vec<&str> = delivery.iter().map(|work| work.0.as_str()).collect();
    assert_eq!(targets, [newer.as_str(), older.as_str()]);
}
