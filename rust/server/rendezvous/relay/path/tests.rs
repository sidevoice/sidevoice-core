use super::*;
use crate::server::rendezvous::test_support::loopback;

#[test]
fn relay_path_never_crosses_local_or_rendezvous_boundary() {
    let base = loopback();
    let local_only = |path: &str| path.starts_with("/api/device/local");
    for path in [
        "/api/presentation/ws",
        "/api/models/catalog",
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
