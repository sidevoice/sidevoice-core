use super::*;
use crate::server::rendezvous::test_support::loopback;

#[test]
fn relay_path_never_crosses_local_or_rendezvous_boundary() {
    let base = loopback();
    let local_only = |path: &str| path.starts_with("/api/device/local");
    for path in [
        "/api/presentation/ws",
        "/api/models/check",
        "/api/host/agents",
        "/api/device/identity",
    ] {
        assert!(relayable(&base, path, local_only), "{path}");
    }
    for path in [
        "/api/connectors/link",
        "/api/rendezvous",
        "/api/device/local/pair",
        "/api/device/%2e%2e/connectors/link",
        "/api/presentation/%252e%252e/connectors",
        "/api/models/../rendezvous",
        "/api/device/..%2f..%2fapi/connectors",
        "/api/device/%5c..%5cconnectors",
        "//evil.example/api/device",
        "/api/device?next=/api/connectors",
    ] {
        assert!(!relayable(&base, path, local_only), "{path}");
    }
    assert!(!crate::server::safe_url("http://room.example"));
}

/// The real local-only rule, however a path is spelled, never reaches the relay.
#[test]
fn the_room_relays_none_of_it_however_it_is_spelled() {
    let base = loopback();
    for path in [
        "/api/device/local",
        "/api/device/local/pair",
        "/api/device/local/",
        "/api/local/health",
        "/api/local",
        "/api/device/%6cocal/pair",
        "/api/device/local%2fpair",
        "/api/device//local/pair",
        "/api/device/./local",
        "/api/device/%2e/local",
        "/api/device/%252e/local/pair",
        "/api/device/x/../local",
        "/api/device/pair/../local",
        "/api/presentation/%2e%2e/local/health",
        "/api/device/%2E/local/pair",
        // The connector link, exactly and spelled other ways.
        "/api/connectors/link",
        "/api/connectors/link/",
        "/api/device/%2e%2e/connectors/link/",
        "/api/presentation/../connectors/link/",
        "/api/connectors/v3",
        // Encoded dot segments never leave the relayed surface.
        "/api/device/%252e%252e/connectors/link/",
        "/api/device/%2e%2e/%2e%2e/api/rendezvous",
        "/api/presentation/%2E%2E%2Fconnectors",
        "/api/device/..%2f..%2fapi",
    ] {
        assert!(!relayable(&base, path, crate::server::local_only), "{path}");
    }
    // What sits beside them under the relayed prefix still is relayed.
    for path in [
        "/api/device/pair",
        "/api/device/localhost",
        "/api/presentation/ws",
    ] {
        assert!(relayable(&base, path, crate::server::local_only), "{path}");
    }
}
