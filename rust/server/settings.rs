//! Settings and try routes backed by the node's existing provider and private stores.

use std::sync::Arc;

use axum::routing::{get, post, put};
use axum::Router;

use super::AppState;

mod catalogs;
mod integrations;
mod preview;
mod provider_errors;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/presentation/languages", get(catalogs::languages))
        .route(
            "/api/presentation/transcription/models",
            get(catalogs::transcription_models),
        )
        .route(
            "/api/presentation/voice-catalog",
            get(catalogs::voice_catalog),
        )
        .route("/api/presentation/integrations", get(integrations::listing))
        .route(
            "/api/presentation/integrations/{provider}",
            put(integrations::save).delete(integrations::clear),
        )
        .route(
            "/api/presentation/synthesis/preview",
            post(preview::synthesis_preview),
        )
}
