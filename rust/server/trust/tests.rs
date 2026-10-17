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
        "http://localhost:10829",
        "http://127.0.0.1/",
        "http://[::1]:10829",
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
    assert_eq!(host_name("[::1]:10829"), Some("::1".into()));
    assert_eq!(host_name("Example.COM:80"), Some("example.com".into()));
    assert_eq!(host_name("localhost"), Some("localhost".into()));
}

#[test]
fn loopback_hosts_are_allowed_and_missing_host_is_not() {
    assert!(host_allowed(&headers(&[(header::HOST, "localhost:10829")])));
    assert!(host_allowed(&headers(&[(header::HOST, "[::1]:10829")])));
    assert!(!host_allowed(&HeaderMap::new()));
}

#[test]
fn numeric_network_hosts_are_allowed_without_trusting_dns_names() {
    for host in [
        "192.168.1.4:10829",
        "10.0.0.42:10829",
        "[fd12::5]:10829",
        "203.0.113.7:443",
    ] {
        assert!(host_allowed(&headers(&[(header::HOST, host)])), "{host}");
    }
    for host in [
        "machine.local:10829",
        "attacker.example:10829",
        "999.1.1.1:10829",
    ] {
        assert!(!host_allowed(&headers(&[(header::HOST, host)])), "{host}");
    }
}

#[test]
fn malformed_host_headers_are_not_allowed() {
    for host in [
        "[fd12::5]junk",
        "[fd12::5]:bad",
        "[fd12::5]:99999",
        "fd12::5",
        "192.168.1.4:bad",
        "192.168.1.4:99999",
        "192.168.1.4:0",
        "192.168.1.4:+80",
        "192.168.1.4/path",
        "192.168.1.4@attacker.example",
        "[fd12::5%eth0]",
        "[fd12::5]:+80",
    ] {
        assert!(!host_allowed(&headers(&[(header::HOST, host)])), "{host}");
    }
}

#[test]
fn explicit_host_allowlist_disables_automatic_numeric_hosts() {
    assert!(host_allowed_by_policy(
        "trusted.example",
        Some("trusted.example"),
        "",
        ""
    ));
    assert!(!host_allowed_by_policy(
        "192.168.1.4",
        Some("trusted.example"),
        "",
        ""
    ));
    assert!(!host_allowed_by_policy("192.168.1.4", Some(""), "", ""));
    assert!(host_allowed_by_policy("192.168.1.4", None, "", ""));
    assert!(
        host_allowed_by_policy("shell.example", Some(""), "https://shell.example", ""),
        "configured Origin hosts remain a separate existing grant"
    );
}

#[test]
fn origins_are_allowed_when_absent_desktop_or_same_as_host() {
    assert!(origin_allowed(&HeaderMap::new()));
    assert!(origin_allowed(&headers(&[(
        header::ORIGIN,
        "tauri://localhost/"
    )])));
    assert!(origin_allowed(&headers(&[
        (header::ORIGIN, "http://192.168.1.4:10829"),
        (header::HOST, "192.168.1.4:10829"),
    ])));
    assert!(!origin_allowed(&headers(&[
        (header::ORIGIN, "http://192.168.1.4:9000"),
        (header::HOST, "192.168.1.4:10829"),
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

/// Every spelling of a local-only path is read as that path.
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
