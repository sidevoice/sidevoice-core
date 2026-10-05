use super::*;

fn pairing() -> Pairing {
    Pairing {
        url: "wss://room.example/link".into(),
        origin: "https://room.example".into(),
        connector_id: "node-1".into(),
        token: "secret".into(),
        dial_key: None,
    }
}

#[test]
fn only_the_route_that_is_up_takes_the_link_down() {
    let mut state = LinkState::default();
    state.connect("dial", None, Some(pairing()));
    assert!(!state.disconnect("outbound"));
    assert_eq!(state.view()["connected"], true);
    assert!(state.disconnect("dial"));
    assert_eq!(state.view()["connected"], false);
    assert!(state.view()["via"].is_null());
}

#[test]
fn public_url_is_kept_only_for_the_linked_pairing() {
    let mut state = LinkState::default();
    state.connect(
        "outbound",
        Some("https://public.example".into()),
        Some(pairing()),
    );
    assert_eq!(
        state.public_url_for(&pairing()),
        Some("https://public.example")
    );
    let other = Pairing {
        token: "rotated".into(),
        ..pairing()
    };
    assert_eq!(state.public_url_for(&other), None);
}

#[test]
fn refusal_is_bounded_and_cleared_by_a_new_pairing() {
    let mut state = LinkState::default();
    state.refuse(&"x".repeat(300));
    assert!(state.is_refused());
    assert_eq!(
        state.view()["refused"].as_str().unwrap().chars().count(),
        200
    );
    state.repaired(Some("https://room.example".into()));
    assert!(!state.is_refused());
    assert_eq!(state.view()["room"], "https://room.example");
}
