//! A paired device's view of the room and its writes into it.

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use super::AppState;

mod focus;
mod input;
mod latency;
mod replay;
mod views;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/presentation", get(views::state))
        .route("/api/presentation/admission", get(views::admission))
        .route("/api/presentation/history", get(views::history))
        .route("/api/presentation/participants", get(views::participants))
        .route("/api/presentation/latency", get(latency::session_latency))
        .route("/api/presentation/select", post(focus::select))
        .route("/api/presentation/leave", post(focus::leave))
        .route("/api/presentation/close", post(focus::close))
        .route("/api/presentation/text", post(input::text))
        .route("/api/presentation/cancel-input", post(input::cancel_input))
        .route("/api/presentation/client-error", post(input::client_error))
        .route("/api/presentation/speak", post(input::speak))
        .route("/api/presentation/replay", post(replay::replay))
}
