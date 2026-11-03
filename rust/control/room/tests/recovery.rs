//! Explicit conversation leave survives a Core restart; transport loss does not become a leave.
use serde_json::json;
use std::collections::HashSet;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tokio::sync::{mpsc, watch};

use crate::control::room::{ConnectorPeer, Room};
use crate::storage::PrivateDir;

fn attach(room: &Room) {
    attach_named(room, "connector");
}
fn attach_named(room: &Room, connector: &str) {
    let (sender, _receiver) = mpsc::channel(4);
    let (stop, _stopped) = watch::channel(false);
    room.attach(
        connector,
        ConnectorPeer {
            generation: "generation".into(),
            sender,
            stop,
        },
    );
}

#[test]
fn an_explicit_room_leave_survives_restart_and_rejects_old_binding_replay() {
    let dir = tempfile::tempdir().unwrap();
    let private = PrivateDir::open(dir.path().join("private")).unwrap();
    let room = Room::load(private.clone()).unwrap();
    attach(&room);
    let id = room
        .register("connector", &json!({"thread":"old","harness":"codex"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    room.close_channel("old").unwrap();
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!(["old"])
    );
    drop(room);

    let room = Room::load(private).unwrap();
    attach(&room);
    let resumed = room.register(
        "connector",
        &json!({"thread":"old","harness":"codex","binding_id":id}),
    );
    assert_eq!(resumed.unwrap_err().key, "room.binding_closed");
    assert!(room.participants(None).as_array().unwrap().is_empty());
    let joined = room
        .register("connector", &json!({"thread":"old","harness":"codex"}))
        .unwrap();
    assert!(
        joined["binding_id"].is_string(),
        "a new explicit join is allowed"
    );
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!([])
    );
}

#[test]
fn a_remembered_conversation_can_be_explicitly_left_while_no_binding_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let private = PrivateDir::open(dir.path().join("private")).unwrap();
    let room = Room::load(private.clone()).unwrap();
    room.close_channel("remembered").unwrap();
    drop(room);
    let room = Room::load(private).unwrap();
    attach(&room);
    assert_eq!(
        room.register(
            "connector",
            &json!({"thread":"remembered","binding_id":"old-binding"})
        )
        .unwrap_err()
        .key,
        "room.binding_closed"
    );
}

#[test]
fn a_user_leave_retires_every_active_binding_of_the_same_thread() {
    let dir = tempfile::tempdir().unwrap();
    let private = PrivateDir::open(dir.path().join("private")).unwrap();
    let room = Room::load(private).unwrap();
    attach(&room);
    attach_named(&room, "other");
    let first = room
        .register("connector", &json!({"thread":"shared"}))
        .unwrap();
    let second = room.register("other", &json!({"thread":"shared"})).unwrap();
    assert_ne!(first["binding_id"], second["binding_id"]);
    assert_eq!(room.participants(None).as_array().unwrap().len(), 2);
    let (_, notifications) = room.close_channel("shared").unwrap();
    assert_eq!(notifications.len(), 2);
    assert!(room.participants(None).as_array().unwrap().is_empty());
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!(["shared"])
    );
}

#[test]
fn a_replayed_binding_cannot_name_another_thread_to_bypass_its_close() {
    let dir = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(dir.path().join("private")).unwrap()).unwrap();
    attach(&room);
    let id = room
        .register("connector", &json!({"thread":"closed"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    room.close_channel("closed").unwrap();
    let replay = room.register("connector", &json!({"thread":"other","binding_id":id}));
    assert_eq!(
        replay.err().map(|error| error.key),
        Some("room.binding_thread_mismatch")
    );
    assert!(room.participants(None).as_array().unwrap().is_empty());
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!(["closed"])
    );
}

#[test]
fn a_foreign_binding_id_cannot_be_used_to_leave_another_connectors_thread() {
    let dir = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(dir.path().join("private")).unwrap()).unwrap();
    attach(&room);
    attach_named(&room, "other");
    let id = room
        .register("other", &json!({"thread":"foreign"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        room.unregister_with_thread("connector", &id, "foreign", true)
            .err()
            .map(|error| error.key),
        Some("room.binding_foreign")
    );
    assert_eq!(room.participants(None).as_array().unwrap().len(), 1);
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!([])
    );
    assert!(room
        .register("other", &json!({"thread":"foreign","binding_id":id}))
        .is_ok());
}

#[test]
fn a_transport_detach_can_replay_the_same_binding_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let private = PrivateDir::open(dir.path().join("private")).unwrap();
    let room = Room::load(private.clone()).unwrap();
    attach(&room);
    let id = room
        .register("connector", &json!({"thread":"old","harness":"codex"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    room.detach("connector", "generation");
    drop(room);
    let room = Room::load(private).unwrap();
    attach(&room);
    assert!(room
        .register(
            "connector",
            &json!({"thread":"old","harness":"codex","binding_id":id})
        )
        .is_ok());
}

#[test]
fn an_agent_leave_is_durable_but_an_ordinary_unregister_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let private = PrivateDir::open(dir.path().join("private")).unwrap();
    let room = Room::load(private.clone()).unwrap();
    attach(&room);
    let old = room
        .register("connector", &json!({"thread":"old"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reset = room
        .register("connector", &json!({"thread":"reset"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    room.unregister_with_thread("connector", &old, "old", true)
        .unwrap();
    room.unregister("connector", &reset);
    drop(room);
    let room = Room::load(private).unwrap();
    attach(&room);
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!(["old"])
    );
    assert_eq!(
        room.register("connector", &json!({"thread":"old","binding_id":old}))
            .unwrap_err()
            .key,
        "room.binding_closed"
    );
    assert!(room
        .register("connector", &json!({"thread":"reset","binding_id":reset}))
        .is_ok());
}

// Unix only: the save is made to fail by a read-only directory (mode 0500); Windows has no such fixture in std (a
// directory's read-only attribute does not stop files being created in it).
#[cfg(unix)]
#[test]
fn a_close_that_cannot_be_saved_does_not_acknowledge_or_retire_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private");
    let private = PrivateDir::open(&path).unwrap();
    let room = Room::load(private).unwrap();
    attach(&room);
    room.register("connector", &json!({"thread":"still-open"}))
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    let closed = room.close_channel("still-open");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        closed.err().map(|error| error.key),
        Some("room.state_save_failed")
    );
    assert_eq!(room.participants(None).as_array().unwrap().len(), 1);
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!([])
    );
}

// Unix only: the save is made to fail by a read-only directory (mode 0500); Windows has no such fixture in std (a
// directory's read-only attribute does not stop files being created in it).
#[cfg(unix)]
#[test]
fn an_agent_leave_is_not_left_listening_when_core_cannot_save_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private");
    let room = Room::load(PrivateDir::open(&path).unwrap()).unwrap();
    attach(&room);
    let id = room
        .register("connector", &json!({"thread":"agent-leaves"}))
        .unwrap()["binding_id"]
        .as_str()
        .unwrap()
        .to_owned();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert_eq!(
        room.unregister_with_thread("connector", &id, "agent-leaves", true)
            .err()
            .map(|error| error.key),
        Some("room.state_save_failed")
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(room.participants(None).as_array().unwrap().is_empty());
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!(["agent-leaves"])
    );
    assert_eq!(
        room.register(
            "connector",
            &json!({"thread":"agent-leaves","binding_id":id})
        )
        .err()
        .map(|error| error.key),
        Some("room.binding_closed")
    );
}

fn open_thread(room: &Room, thread: &str) {
    attach(room);
    room.register("connector", &json!({"thread":thread}))
        .unwrap();
}

fn devices(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

fn listed(room: &Room, device: &str, recent: &HashSet<String>) -> usize {
    room.participant_inventory_for(None, Some(device), recent)
        .unwrap()["participants"]
        .as_array()
        .unwrap()
        .len()
}

#[test]
fn a_device_removing_a_conversation_keeps_it_open_for_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(dir.path().join("private")).unwrap()).unwrap();
    let recent = devices(&["phone", "laptop"]);
    open_thread(&room, "shared");
    listed(&room, "phone", &recent);
    let (result, notify) = room.remove_for_device("shared", "phone", &recent).unwrap();
    assert_eq!(result["closed"], json!(false));
    assert!(notify.is_empty());
    assert_eq!(listed(&room, "phone", &recent), 0);
    assert_eq!(listed(&room, "laptop", &recent), 1);
}

#[test]
fn a_conversation_with_no_record_is_shown_by_every_device_that_counts() {
    let dir = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(dir.path().join("private")).unwrap()).unwrap();
    open_thread(&room, "shared");
    let recent = devices(&["phone", "laptop"]);
    let (result, _) = room.remove_for_device("shared", "phone", &recent).unwrap();
    assert_eq!(result["closed"], json!(false), "the laptop was never asked");
}

#[test]
fn the_last_device_removing_a_conversation_closes_it_the_normal_way_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let private = PrivateDir::open(dir.path().join("private")).unwrap();
    let room = Room::load(private.clone()).unwrap();
    let recent = devices(&["phone", "laptop"]);
    open_thread(&room, "shared");
    listed(&room, "phone", &recent);
    room.remove_for_device("shared", "phone", &recent).unwrap();
    let (result, notify) = room.remove_for_device("shared", "laptop", &recent).unwrap();
    assert_eq!(result["closed"], json!(true));
    assert_eq!(notify.len(), 1);
    assert_eq!(notify[0].1["reason"], "closed_from_room");
    assert!(room.participants(None).as_array().unwrap().is_empty());
    let restarted = Room::load(private).unwrap();
    assert_eq!(
        restarted.participant_inventory(None)["closed_threads"],
        json!(["shared"])
    );
}

#[test]
fn a_device_that_no_longer_counts_does_not_keep_a_conversation_open() {
    let dir = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(dir.path().join("private")).unwrap()).unwrap();
    open_thread(&room, "shared");
    listed(&room, "phone", &devices(&["phone", "laptop"]));
    let (result, _) = room
        .remove_for_device("shared", "phone", &devices(&["phone"]))
        .unwrap();
    assert_eq!(
        result["closed"],
        json!(true),
        "the laptop is unpaired or stale"
    );
}

#[test]
fn the_last_viewer_aging_out_closes_the_conversation_without_any_removal() {
    let dir = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(dir.path().join("private")).unwrap()).unwrap();
    open_thread(&room, "shared");
    listed(&room, "laptop", &devices(&["laptop"]));
    assert!(room.expire_departed(&devices(&["laptop"])).is_empty());
    assert_eq!(room.participants(None).as_array().unwrap().len(), 1);
    let notify = room.expire_departed(&devices(&[]));
    assert_eq!(notify.len(), 1);
    assert!(room.participants(None).as_array().unwrap().is_empty());
    assert_eq!(
        room.participant_inventory(None)["closed_threads"],
        json!(["shared"])
    );
}

#[test]
fn connecting_voice_anew_undoes_removals_but_keeps_who_still_shows_it_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let private = PrivateDir::open(dir.path().join("private")).unwrap();
    let room = Room::load(private.clone()).unwrap();
    let recent = devices(&["phone", "laptop"]);
    open_thread(&room, "shared");
    listed(&room, "phone", &recent);
    room.remove_for_device("shared", "phone", &recent).unwrap();
    room.register("connector", &json!({"thread":"shared"}))
        .unwrap();
    assert_eq!(listed(&room, "phone", &recent), 1);
    // After a restart the phone is back, so the laptop removing it does not close it.
    let restarted = Room::load(private).unwrap();
    let (result, _) = restarted
        .remove_for_device("shared", "laptop", &recent)
        .unwrap();
    assert_eq!(result["closed"], json!(false));
}

#[cfg(unix)]
#[test]
fn a_viewer_that_could_not_be_written_is_refused_and_written_by_the_next_answer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private");
    let room = Room::load(PrivateDir::open(&path).unwrap()).unwrap();
    let recent = devices(&["phone"]);
    open_thread(&room, "shared");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    let refused = room.participant_inventory_for(None, Some("phone"), &recent);
    assert_eq!(refused.err().map(|e| e.key), Some("room.state_save_failed"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(listed(&room, "phone", &recent), 1);
    let state = std::fs::read_to_string(path.join("room-state.json")).unwrap();
    assert!(state.contains("conversation_devices") && state.contains("phone"));
}

#[cfg(unix)]
#[test]
fn a_reconnect_whose_undoing_could_not_be_written_is_not_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private");
    let room = Room::load(PrivateDir::open(&path).unwrap()).unwrap();
    let recent = devices(&["phone", "laptop"]);
    open_thread(&room, "shared");
    listed(&room, "phone", &recent);
    room.remove_for_device("shared", "phone", &recent).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    let refused = room.register("connector", &json!({"thread":"shared"}));
    assert_eq!(refused.err().map(|e| e.key), Some("room.state_save_failed"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    room.register("connector", &json!({"thread":"shared"}))
        .unwrap();
    assert_eq!(listed(&room, "phone", &recent), 1);
}

#[cfg(unix)]
#[test]
fn a_sweep_close_that_could_not_be_written_leaves_the_conversation_for_the_next_sweep() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private");
    let room = Room::load(PrivateDir::open(&path).unwrap()).unwrap();
    open_thread(&room, "shared");
    listed(&room, "laptop", &devices(&["laptop"]));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(room.expire_departed(&devices(&[])).is_empty());
    assert_eq!(room.participants(None).as_array().unwrap().len(), 1);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(room.expire_departed(&devices(&[])).len(), 1);
    assert!(room.participants(None).as_array().unwrap().is_empty());
}

#[test]
fn removing_a_conversation_with_an_invalid_thread_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let room = Room::load(PrivateDir::open(dir.path().join("private")).unwrap()).unwrap();
    assert_eq!(
        room.remove_for_device("", "phone", &devices(&["phone"]))
            .err()
            .map(|e| e.key),
        Some("room.thread_invalid")
    );
}
