//! Localized refusals: an i18n key and a status, rendered in the client's language.

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map};

use crate::control::room::RoomError;
use crate::messages::{render, LocalizedMessage};

use super::request::accept_language;
use super::trust::origin_allowed;

/// A rendered refusal leaving a handler early, boxed to keep handler results small.
pub(super) struct Refusal(Box<Response>);

impl From<Response> for Refusal {
    fn from(response: Response) -> Self {
        Self(Box::new(response))
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        *self.0
    }
}

/// A handler's answer, or the refusal that ended it early.
pub(super) type Handled = Result<Response, Refusal>;

pub(super) fn failure(key: &str, status: StatusCode, headers: &HeaderMap) -> Response {
    let message = render(
        &LocalizedMessage {
            key: key.to_owned(),
            params: Map::new(),
        },
        accept_language(headers),
    );
    let mut response = (status, Json(json!({"detail": message}))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

pub(super) fn refuse(key: &str, status: StatusCode, headers: &HeaderMap) -> Refusal {
    failure(key, status, headers).into()
}

pub(super) fn room_refusal(error: RoomError, headers: &HeaderMap) -> Refusal {
    refuse(
        error.key,
        StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST),
        headers,
    )
}

/// Refuses a browser request whose `Origin` the node does not trust.
pub(super) fn require_origin(headers: &HeaderMap) -> Result<(), Refusal> {
    if origin_allowed(headers) {
        Ok(())
    } else {
        Err(refuse(
            "request.origin_invalid",
            StatusCode::FORBIDDEN,
            headers,
        ))
    }
}

pub(super) async fn not_found(headers: HeaderMap) -> Response {
    failure("request.not_found", StatusCode::NOT_FOUND, &headers)
}
