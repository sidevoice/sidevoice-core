use super::Offer;

fn offer(session_id: &str, sdp: &str, kind: &str) -> Offer {
    Offer {
        session_id: session_id.into(),
        sdp: sdp.into(),
        kind: kind.into(),
    }
}

#[test]
fn offers_are_bounded_and_typed() {
    assert!(offer("session", "v=0", "offer").is_valid());
    assert!(offer(&"s".repeat(64), &"v".repeat(64_000), "offer").is_valid());
    assert!(!offer("session", "v=0", "answer").is_valid());
    assert!(!offer("", "v=0", "offer").is_valid());
    assert!(!offer(&"s".repeat(65), "v=0", "offer").is_valid());
    assert!(!offer("session", "", "offer").is_valid());
    assert!(!offer("session", &"v".repeat(64_000 + 1), "offer").is_valid());
}
