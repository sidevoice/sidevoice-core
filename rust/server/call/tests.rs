use std::sync::Arc;

use crate::control::room::Room;
use crate::server::tests::support::{app_state, private_dir};

use super::registration::CallRegistration;

#[test]
fn repeated_browser_refusals_release_registration_senders() {
    let (_temp, dir) = private_dir();
    let room = Arc::new(Room::load(dir.clone()).unwrap());
    let state = app_state(&dir, room, "host");
    for _ in 0..64 {
        let registration = CallRegistration::new(state.clone(), "device".into());
        assert_eq!(state.calls.signals("device"), Some(1));
        drop(registration);
        assert_eq!(state.calls.signals("device"), None);
    }
    let first = CallRegistration::new(state.clone(), "device".into());
    let second = CallRegistration::new(state.clone(), "device".into());
    drop(first);
    assert_eq!(state.calls.signals("device"), Some(1));
    drop(second);
    assert_eq!(state.calls.signals("device"), None);
}
