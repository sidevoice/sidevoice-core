//! Minimum device trust surface on TCP and the same user's Unix socket.

use std::sync::{Arc, Mutex};

use axum::middleware;

use axum::Router;
use serde_json::{json, Value};

use crate::control::devices::{DeviceRegistry, NodeIdentity};
use crate::control::room::Room;
use crate::storage::PrivateDir;

mod call;
mod connector_routes;
mod connectors;
mod device_address;
mod guard;
mod limits;
mod node;
mod pairing;
mod presentation;
mod refusal;
mod request;
#[cfg(test)]
mod tests;
mod trust;

pub(crate) use trust::safe_url;

pub struct AppState {
    pub dir: PrivateDir,
    pub room: Arc<Room>,
    pub identity: NodeIdentity,
    registry: Mutex<DeviceRegistry>,
    limits: limits::Limits,
    calls: call::CallRegistry,
    /// Every live call a page may come back to, and the client messages already taken.
    resumable: call::ResumableCalls,
    seen: call::SeenMessages,
    launch_id: String,
    host: String,
    bind_host: String,
    port: u16,
}

#[derive(Clone)]
struct AuthenticatedDevice(String);

impl AppState {
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
        bind_host: String,
        port: u16,
        room: Arc<Room>,
    ) -> Self {
        Self {
            dir,
            room,
            identity,
            registry: Mutex::new(registry),
            limits: limits::Limits::default(),
            calls: call::CallRegistry::default(),
            resumable: call::ResumableCalls::default(),
            seen: call::SeenMessages::default(),
            launch_id,
            host,
            bind_host,
            port,
        }
    }

    pub fn issue_code(&self) -> Value {
        let private_urls = device_address::code_urls(&self.bind_host, self.port);
        // Keep configured public routes ahead of the discovered LAN routes: older clients read at most eight URLs.
        let mut urls = vec![private_urls[0].clone()];
        for value in std::env::var("SIDEVOICE_PUBLIC_URLS")
            .unwrap_or_default()
            .split(',')
        {
            let value = value.trim().trim_end_matches('/');
            if !value.is_empty() && safe_url(value) && !urls.iter().any(|url| url == value) {
                urls.push(value.to_owned());
            }
        }
        for url in private_urls.into_iter().skip(1) {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
        self.registry.lock().expect("registry lock").issue_code(
            &self.identity,
            Some(&self.host),
            &urls,
        )
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
    let router = router.layer(middleware::from_fn_with_state((state, local), guard::guard));
    match telemetry {
        Some(telemetry) => router.layer(middleware::from_fn_with_state(
            telemetry,
            crate::control::telemetry::http_span,
        )),
        None => router,
    }
}
