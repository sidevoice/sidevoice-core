//! Browser WebRTC audio offer/answer; the call socket remains the control path.

mod handler;
mod peer;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use self::{handler::Handler, peer::ANSWER_FAILED};
use super::{failure, AppState, AuthenticatedDevice};

#[derive(Deserialize)]
pub(super) struct Offer {
    session_id: String,
    sdp: String,
    #[serde(rename = "type")]
    kind: String,
}

impl Offer {
    fn is_valid(&self) -> bool {
        self.kind == "offer"
            && !self.session_id.is_empty()
            && self.session_id.len() <= 64
            && !self.sdp.is_empty()
            && self.sdp.len() <= 64_000
    }
}

fn enabled() -> bool {
    !matches!(
        std::env::var("SIDEVOICE_WEBRTC")
            .unwrap_or_else(|_| "on".into())
            .to_ascii_lowercase()
            .as_str(),
        "off" | "0" | "false"
    )
}

fn ice_urls() -> Vec<String> {
    match std::env::var("SIDEVOICE_STUN_URLS") {
        Ok(urls) => urls
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => vec!["stun:stun.l.google.com:19302".into()],
    }
}

pub(super) async fn config() -> Json<Value> {
    let urls = ice_urls();
    Json(
        json!({"enabled":enabled(),"ice_servers":if enabled() && !urls.is_empty() { vec![json!({"urls":urls})] } else { vec![] }}),
    )
}

pub(super) async fn offer(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    Json(offer): Json<Offer>,
) -> Response {
    if !super::trust::origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    if !enabled() {
        return failure(
            "voice.rtc_unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers,
        );
    }
    if !offer.is_valid() {
        return failure(
            "room.request_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        );
    }
    if !state.room.owns_session(&offer.session_id, &device.0) {
        return failure("room.browser_absent", StatusCode::CONFLICT, &headers);
    }
    let call = state
        .media
        .lock()
        .expect("media lock")
        .get(&offer.session_id)
        .cloned();
    let Some(call) = call else {
        return failure("room.browser_absent", StatusCode::CONFLICT, &headers);
    };
    let generation = call.replace_rtc().await;
    let (handler, gathered) = Handler::new(&call, generation);
    let peer = match peer::build(handler, ice_urls()).await {
        Ok(peer) => peer,
        Err(error) => {
            eprintln!("{error}");
            return failure(ANSWER_FAILED, StatusCode::UNPROCESSABLE_ENTITY, &headers);
        }
    };
    let answer = match peer::answer(&peer, offer.sdp, gathered).await {
        Ok(answer) => answer,
        Err(error) => {
            eprintln!("WebRTC answer failed: {error}");
            let _ = peer.close().await;
            return failure(ANSWER_FAILED, StatusCode::UNPROCESSABLE_ENTITY, &headers);
        }
    };
    call.set_rtc(generation, peer).await;
    Json(json!({"sdp":answer.sdp,"type":"answer"})).into_response()
}
