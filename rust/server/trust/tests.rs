use axum::http::HeaderValue;

use super::*;

fn headers(pairs: &[(header::HeaderName, &'static str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(name.clone(), HeaderValue::from_static(value));
    }
    headers
}

#[test]
fn local_paths_survive_encoding_and_dot_segments() {
    for path in [
        "/api/local/health",
        "/api/device/local",
        "/api/connectors/v3",
        "/api/presentation/../local/health",
        "/api/%2e%2e/api/connectors/link/",
        "/api/%252e%252e/api/device/local/pair",
        "//api/./local",
    ] {
        assert!(local_only(path), "{path}");
    }
    for path in [
        "/api/device/localhost",
        "/api/presentation",
        "/api/connectors",
        "/api/local/../presentation",
    ] {
        assert!(!local_only(path), "{path}");
    }
}

#[test]
fn safe_urls_are_https_or_loopback_http() {
    for url in [
        "https://room.example",
        "http://localhost:8768",
        "http://127.0.0.1/",
        "http://[::1]:8768",
        "http://LOCALHOST./",
    ] {
        assert!(safe_url(url), "{url}");
    }
    for url in ["ftp://localhost", "not a url", "file:///tmp"] {
        assert!(!safe_url(url), "{url}");
    }
}

#[test]
fn host_name_drops_port_and_brackets() {
    assert_eq!(host_name("[::1]:8768"), "::1");
    assert_eq!(host_name("Example.COM:80"), "example.com");
    assert_eq!(host_name("localhost"), "localhost");
}

#[test]
fn loopback_hosts_are_allowed_and_missing_host_is_not() {
    assert!(host_allowed(&headers(&[(header::HOST, "localhost:8768")])));
    assert!(host_allowed(&headers(&[(header::HOST, "[::1]:8768")])));
    assert!(!host_allowed(&HeaderMap::new()));
}

#[test]
fn origins_are_allowed_when_absent_desktop_or_same_as_host() {
    assert!(origin_allowed(&HeaderMap::new()));
    assert!(origin_allowed(&headers(&[(
        header::ORIGIN,
        "tauri://localhost/"
    )])));
    assert!(origin_allowed(&headers(&[
        (header::ORIGIN, "http://192.168.1.4:8768"),
        (header::HOST, "192.168.1.4:8768"),
    ])));
    assert!(!origin_allowed(&headers(&[
        (header::ORIGIN, "http://192.168.1.4:9000"),
        (header::HOST, "192.168.1.4:8768"),
    ])));
    assert!(!origin_allowed(&headers(&[(
        header::ORIGIN,
        "https://elsewhere.example"
    )])));
}

#[test]
fn credentials_travel_only_over_a_safe_transport() {
    for safe in [
        "https://room.example",
        "wss://room.example",
        "http://127.0.0.1:8080",
        "ws://localhost:9",
        "http://[::1]:1",
    ] {
        assert!(credential_safe(safe), "{safe}");
    }
    for unsafe_url in [
        "http://room.example",
        "ws://room.example",
        "ftp://room.example",
        "nonsense",
    ] {
        assert!(!credential_safe(unsafe_url), "{unsafe_url}");
    }
}

/// Spellings ported from the Python suite (`tests/test_local_socket.py` `LocalOnlyRuleTests`).
#[test]
fn the_local_only_rule_reads_the_path_however_it_is_spelled() {
    for path in [
        "/api/local",
        "/api/local/health",
        "/api/device/local",
        "/api/device/local/pair",
        "/api//device/local",
        "/api/./device/local/",
        "/api/connectors/link",
        "/api/connectors/link/",
        "/api/connectors/v3",
        "/api/connectors//v3",
        "/api/connectors/./v3/",
        "/api/device/%6cocal/pair",
        "/api/device/%252e/local/pair",
        "/api/device/x/../local",
    ] {
        assert!(local_only(path), "{path}");
    }
    for path in [
        "/api/device/pair",
        "/api/localhost",
        "/api/device/localx",
        "/api/connectors",
        "/api/presentation/ws",
    ] {
        assert!(!local_only(path), "{path}");
    }
}
