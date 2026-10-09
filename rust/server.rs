//! Minimum device trust surface on TCP and the same user's Unix socket.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use serde_json::{json, Value};

use crate::control::devices::{DeviceRegistry, NodeIdentity};
use crate::control::room::Room;
use crate::providers::cache::SynthesisCache;
use crate::storage::PrivateDir;

mod call;
mod connector_routes;
mod connectors;
mod guard;
mod media;
mod model_check;
mod node;
mod pairing;
mod presentation;
mod refusal;
pub mod rendezvous;
mod request;
mod rtc;
mod settings;
#[cfg(test)]
mod tests;
mod transcription_trial;
mod trust;

// The handler toolkit the sibling route modules reach through `super::`.
use refusal::failure;
use request::payload;
use trust::origin_allowed;
pub(crate) use trust::{credential_safe, local_only, safe_url};

/// The room a pairing code may name, only when the device's pairing secret and token may travel to it.
fn advertisable_room(room: Option<Value>) -> Option<Value> {
    room.filter(|room| room["url"].as_str().is_some_and(credential_safe))
}

pub struct AppState {
    pub dir: PrivateDir,
    pub room: Arc<Room>,
    pub rendezvous: Arc<rendezvous::Rendezvous>,
    pub identity: NodeIdentity,
    registry: Mutex<DeviceRegistry>,
    calls: call::CallRegistry,
    /// Every live call a page may come back to, and the client messages already taken.
    resumable: call::ResumableCalls,
    seen: call::SeenMessages,
    media: Mutex<HashMap<String, Arc<media::CallMedia>>>,
    cancel_input: Mutex<HashMap<String, tokio::sync::mpsc::Sender<u64>>>,
    call_settings: Mutex<HashMap<String, crate::types::CallSettings>>,
    replay_audio: Mutex<HashMap<String, Arc<PinnedReplay>>>,
    check_budget: model_check::CheckBudget,
    trial_budget: transcription_trial::TrialBudget,
    integration_revisions: Mutex<HashMap<String, u64>>,
    synthesis: Arc<SynthesisCache>,
    launch_id: String,
    host: String,
    port: u16,
}

struct PinnedReplay {
    speech: Arc<crate::providers::CloudSpeech>,
    voice: crate::models::ResolvedVoice,
}

#[derive(Clone)]
struct AuthenticatedDevice(String);

impl AppState {
    fn prune_replay_audio(&self) {
        self.replay_audio
            .lock()
            .expect("replay audio lock")
            .retain(|uid, _| self.room.has_replay(uid));
    }

    fn retire_session_replays(&self, session: &str) {
        // Replay admission also takes the audio lock before entering the room.
        // Keep leave and the final purge atomic with that admission path.
        let mut audio = self.replay_audio.lock().expect("replay audio lock");
        self.room.leave(session);
        audio.retain(|uid, _| {
            !uid.starts_with(&format!("{session}:replay:")) && self.room.has_replay(uid)
        });
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "shared application owners are explicit at construction"
    )]
    pub fn new(
        dir: PrivateDir,
        identity: NodeIdentity,
        registry: DeviceRegistry,
        launch_id: String,
        host: String,
        port: u16,
        room: Arc<Room>,
        rendezvous: Arc<rendezvous::Rendezvous>,
    ) -> Self {
        Self {
            dir,
            room,
            rendezvous,
            identity,
            registry: Mutex::new(registry),
            calls: call::CallRegistry::default(),
            resumable: call::ResumableCalls::default(),
            seen: call::SeenMessages::default(),
            media: Mutex::new(HashMap::new()),
            cancel_input: Mutex::new(HashMap::new()),
            call_settings: Mutex::new(HashMap::new()),
            replay_audio: Mutex::new(HashMap::new()),
            check_budget: model_check::CheckBudget::default(),
            trial_budget: transcription_trial::TrialBudget::default(),
            integration_revisions: Mutex::new(HashMap::new()),
            synthesis: Arc::new(SynthesisCache::new()),
            launch_id,
            host,
            port,
        }
    }

    pub fn issue_code(&self) -> Value {
        let mut urls = vec![format!("http://127.0.0.1:{}", self.port)];
        for value in std::env::var("SIDEVOICE_PUBLIC_URLS")
            .unwrap_or_default()
            .split(',')
        {
            let value = value.trim().trim_end_matches('/');
            if !value.is_empty() && safe_url(value) && !urls.iter().any(|url| url == value) {
                urls.push(value.to_owned());
            }
        }
        let mut code = self.registry.lock().expect("registry lock").issue_code(
            &self.identity,
            Some(&self.host),
            &urls,
        );
        if let Some(rv) = advertisable_room(self.rendezvous.room_for_devices()) {
            code["payload"]["rv"] = rv;
            let payload = serde_json::to_vec(&code["payload"]).expect("pairing payload");
            code["code"] = json!(format!(
                "SV1.{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
            ));
        }
        code
    }

    pub fn open_calls(&self) -> usize {
        self.calls.open()
    }

    pub fn authenticate_token(&self, token: &str) -> Option<String> {
        self.registry
            .lock()
            .expect("registry lock")
            .authenticate(token)
    }

    pub fn close_calls(&self, id: &str) {
        self.calls.close(id);
    }

    fn node(&self) -> Value {
        json!({"fingerprint": self.identity.fingerprint, "public_key": self.identity.public_key, "host": self.host})
    }
}

pub fn router(state: Arc<AppState>, local: bool) -> Router {
    let mut router = Router::new()
        .merge(node::routes())
        .merge(pairing::routes())
        .merge(call::routes())
        .merge(settings::routes())
        .route("/api/models/check", post(model_check::model_check))
        .route(
            "/api/models/transcription/preview",
            post(transcription_trial::preview),
        )
        .route("/api/presentation/rtc/config", get(rtc::config))
        .route("/api/presentation/rtc/offer", post(rtc::offer))
        .merge(presentation::routes())
        .merge(connector_routes::routes());
    if local {
        router = router
            .merge(node::local_routes())
            .merge(pairing::local_routes())
            .merge(connectors::local_routes());
    }
    let telemetry = crate::control::telemetry::shared();
    if telemetry.is_some() {
        router = router.route_layer(middleware::from_fn(crate::control::telemetry::http_route));
    }
    let router = router
        .fallback(refusal::not_found)
        .with_state(state.clone());
    let router = if local {
        connectors::v2_layer(router, state.clone())
    } else {
        rendezvous::layer(router, state.clone())
    };
    let router = router.layer(middleware::from_fn_with_state((state, local), guard::guard));
    match telemetry {
        Some(telemetry) => router.layer(middleware::from_fn_with_state(
            telemetry,
            crate::control::telemetry::http_span,
        )),
        None => router,
    }
}
