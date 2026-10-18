//! The middleware every request passes: host and local-path checks, CORS, and
//! bearer authentication of the paired device outside the open routes.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::refusal::failure;
use super::trust::{host_allowed, local_only, origin_allowed};
use super::{AppState, AuthenticatedDevice};

pub(super) async fn guard(
    State((state, local)): State<(Arc<AppState>, bool)>,
    mut request: Request,
    next: Next,
) -> Response {
    let headers = request.headers().clone();
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |address| address.0.ip());
    let pairing = !local && method == Method::POST && path == "/api/device/pair";
    let socket_upgrade = call_socket_upgrade(&method, &path, &headers);
    let protected = !open_route(&method, &path, local) && !socket_upgrade;
    let device = if protected {
        bearer_token(&headers).and_then(|token| state.authenticate_token(token))
    } else {
        None
    };
    let auth_failed = !local
        && (!host_allowed(&headers)
            || !origin_allowed(&headers)
            || (protected && device.is_none())
            || (socket_upgrade
                && socket_token(&headers)
                    .and_then(|token| state.authenticate_token(token))
                    .is_none()));
    if pairing {
        if let Some(mut response) = state.limits.check_pair(peer, &headers) {
            cors(&mut response, &headers);
            expose_retry_after(&mut response);
            return response;
        }
    }
    if auth_failed {
        if let Some(mut response) = state.limits.check_auth(peer, &headers) {
            cors(&mut response, &headers);
            expose_retry_after(&mut response);
            return response;
        }
        state.limits.failed_auth(peer);
    }
    if !host_allowed(&headers) {
        return failure(
            "request.host_invalid",
            StatusCode::MISDIRECTED_REQUEST,
            &headers,
        );
    }
    if local_only(&path) && (!local || headers.contains_key(header::ORIGIN)) {
        return failure("request.not_found", StatusCode::NOT_FOUND, &headers);
    }
    if method == Method::OPTIONS && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD) {
        return preflight(&headers);
    }
    if protected {
        let Some(device) = device else {
            let mut response = failure("device.unpaired", StatusCode::UNAUTHORIZED, &headers);
            cors(&mut response, &headers);
            return response;
        };
        request.extensions_mut().insert(AuthenticatedDevice(device));
    }
    let mut response = next.run(request).await;
    if pairing && response.status().is_client_error() {
        state.limits.failed_pair(peer);
    }
    cors(&mut response, &headers);
    response
}

fn expose_retry_after(response: &mut Response) {
    response.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Retry-After"),
    );
}

fn socket_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim)
        .find_map(|value| value.strip_prefix("sidevoice.token."))
}

/// Routes that answer without a paired device's bearer token. The room's dialling link carries its own key, and
/// Socket.IO long-polling POSTs to it too, so any method under it is open.
pub(super) fn open_route(method: &Method, path: &str, local: bool) -> bool {
    (method == Method::GET && matches!(path, "/api/rendezvous" | "/api/device/identity"))
        || path == "/api/rendezvous/link"
        || path.starts_with("/api/rendezvous/link/")
        || (method == Method::POST && path == "/api/device/pair")
        || (local && local_only(path))
}

/// The call socket authenticates through its subprotocol, not a header.
fn call_socket_upgrade(method: &Method, path: &str, headers: &HeaderMap) -> bool {
    method == Method::GET
        && path == "/api/presentation/ws"
        && headers
            .get(header::UPGRADE)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then_some(token.trim())
                .filter(|v| !v.is_empty())
        })
}

fn preflight(headers: &HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    cors(&mut response, headers);
    if !response
        .headers()
        .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    {
        return response;
    }
    let granted = response.headers_mut();
    if headers
        .get("access-control-request-private-network")
        .is_some_and(|value| value == "true")
    {
        granted.insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
    }
    granted.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE"),
    );
    granted.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, accept, authorization"),
    );
    granted.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}

fn cors(response: &mut Response, headers: &HeaderMap) {
    if !origin_allowed(headers) {
        return;
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Origin"));
    }
}
