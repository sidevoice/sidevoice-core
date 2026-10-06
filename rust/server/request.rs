//! Reading what a request carries: its language, query parameters and JSON body.

use axum::body::Bytes;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use serde_json::Value;

use super::refusal::{refuse, Refusal};

/// The first language the client accepts, or English.
pub(super) fn accept_language(headers: &HeaderMap) -> &str {
    headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .unwrap_or("en")
}

pub(super) fn payload(body: &Bytes) -> Option<Value> {
    serde_json::from_slice(body).ok().filter(Value::is_object)
}

/// The JSON object body of a room request, or its refusal.
pub(super) fn room_payload(body: &Bytes, headers: &HeaderMap) -> Result<Value, Refusal> {
    payload(body).ok_or_else(|| refuse("room.request_invalid", StatusCode::BAD_REQUEST, headers))
}

pub(super) fn query(uri: &Uri, name: &str) -> Option<String> {
    uri.query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    })
}
