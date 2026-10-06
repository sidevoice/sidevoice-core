//! Which i18n key a provider failure is reported under.

use axum::http::{HeaderMap, StatusCode};

use crate::messages::{render, LocalizedMessage};
use crate::providers::{ProviderError, ProviderErrorKind};
use crate::server::refusal::{refuse, Refusal};
use crate::server::request::accept_language;

/// A provider refusal while verifying a key or trying a voice.
pub(super) fn provider_refusal(error: &ProviderError, headers: &HeaderMap) -> Refusal {
    refuse(
        failure_key(error, "integration.verify_failed"),
        StatusCode::UNPROCESSABLE_ENTITY,
        headers,
    )
}

/// A rendered message for a catalogue the provider could not list.
pub(super) fn catalog_error(error: &ProviderError, headers: &HeaderMap) -> String {
    render(
        &LocalizedMessage::new(failure_key(error, "integration.catalog_failed")),
        accept_language(headers),
    )
}

fn failure_key(error: &ProviderError, otherwise: &'static str) -> &'static str {
    match error.kind {
        ProviderErrorKind::Unauthorized => "integration.key_refused",
        ProviderErrorKind::Timeout | ProviderErrorKind::Transport => "integration.unreachable",
        _ => otherwise,
    }
}
