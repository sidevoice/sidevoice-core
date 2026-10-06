//! Which URLs, paths, origins and hosts the node trusts, from its environment.

use axum::http::{header, HeaderMap};
use percent_encoding::percent_decode_str;
use url::Url;

/// Paths reserved for the same user's Unix listener.
const LOCAL_PREFIXES: [&str; 4] = [
    "/api/local",
    "/api/device/local",
    "/api/connectors/link",
    "/api/connectors/v3",
];

const DESKTOP_ORIGINS: [&str; 3] = [
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];

pub(crate) fn safe_url(value: &str) -> bool {
    let Ok(parsed) = Url::parse(value) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if parsed.scheme() == "https" {
        return true;
    }
    if parsed.scheme() != "http" {
        return false;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1") {
        return true;
    }
    trusted_cluster_host(&host)
}

/// Whether a credential may travel to `value`: https or wss anywhere, http or ws only to loopback or a trusted
/// cluster host (`safe_url`).
pub(crate) fn credential_safe(value: &str) -> bool {
    let Ok(mut parsed) = Url::parse(value) else {
        return false;
    };
    let scheme = match parsed.scheme() {
        "wss" => "https",
        "ws" => "http",
        other => other,
    }
    .to_owned();
    parsed.set_scheme(&scheme).is_ok() && safe_url(parsed.as_str())
}

fn trusted_cluster_host(host: &str) -> bool {
    std::env::var("SIDEVOICE_TRUSTED_CLUSTER_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry.starts_with('.') {
                host.ends_with(&entry)
            } else {
                host == entry
            }
        })
}

pub(crate) fn local_only(path: &str) -> bool {
    let normalized = normalize_path(path);
    LOCAL_PREFIXES
        .iter()
        .any(|prefix| normalized == *prefix || normalized.starts_with(&format!("{prefix}/")))
}

/// Decodes twice and resolves dot segments, so encoded traversal cannot hide a prefix.
fn normalize_path(path: &str) -> String {
    let mut decoded = path.to_owned();
    for _ in 0..2 {
        decoded = percent_decode_str(&decoded).decode_utf8_lossy().to_string();
    }
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => (),
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    format!("/{}", segments.join("/"))
}

pub(super) fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let origin = origin.trim_end_matches('/');
    let public_origin = std::env::var("VOICE_PUBLIC_ORIGIN").unwrap_or_default();
    let configured = std::env::var("SIDEVOICE_ALLOWED_ORIGINS").unwrap_or_default();
    let allowed = DESKTOP_ORIGINS
        .into_iter()
        .chain(public_origin.split(',').map(str::trim))
        .chain(configured.split(',').map(str::trim))
        .any(|item| !item.is_empty() && item.trim_end_matches('/') == origin);
    allowed || same_origin_as_host(origin, headers)
}

fn same_origin_as_host(origin: &str, headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let own = Url::parse(&format!("http://{host}")).ok();
    Url::parse(origin)
        .ok()
        .zip(own)
        .is_some_and(|(origin, own)| {
            origin.host_str() == own.host_str() && origin.port() == own.port()
        })
}

pub(super) fn host_allowed(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let host = host_name(value);
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    if std::env::var("SIDEVOICE_ALLOWED_HOSTS")
        .unwrap_or_default()
        .split(',')
        .any(|item| item.trim().eq_ignore_ascii_case(&host))
    {
        return true;
    }
    let configured = std::env::var("SIDEVOICE_ALLOWED_ORIGINS").unwrap_or_default();
    let public_origin = std::env::var("VOICE_PUBLIC_ORIGIN").unwrap_or_default();
    configured
        .split(',')
        .chain(public_origin.split(','))
        .any(|origin| {
            Url::parse(origin.trim())
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|name| name.eq_ignore_ascii_case(&host))
        })
}

/// The lowercase host of a `Host` header value, without port or IPv6 brackets.
fn host_name(value: &str) -> String {
    if value.starts_with('[') {
        value
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        value.split(':').next().unwrap_or("")
    }
    .to_ascii_lowercase()
}

#[cfg(test)]
mod tests;
