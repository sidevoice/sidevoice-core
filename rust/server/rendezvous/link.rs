//! The link's lifecycle: watch the pairing file, keep one route to the room
//! up, and report every change of state to the room.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use socketioxide::extract::SocketRef;
use tokio::sync::futures::Notified;
use tokio::sync::{watch, Mutex, Notify};
use url::Url;

use super::client;
use super::pairing::Pairing;
use super::state::LinkState;
use crate::control::room::Room;
use crate::messages::{render, LocalizedMessage};

const PROTOCOL: u8 = 3;
const FIRST_RETRY: Duration = Duration::from_millis(250);
const LAST_RETRY: Duration = Duration::from_secs(10);
const IDLE_POLL: Duration = Duration::from_secs(2);

pub struct Rendezvous {
    pairing_path: Option<PathBuf>,
    base: Url,
    host: String,
    room: Arc<Room>,
    state: StdMutex<LinkState>,
    dialled: Mutex<HashMap<String, SocketRef>>,
    changed: Notify,
    stopping: watch::Sender<bool>,
}

impl Rendezvous {
    pub fn new(
        pairing_path: Option<PathBuf>,
        base: Url,
        host: String,
        room: Arc<Room>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pairing_path,
            base,
            host,
            room,
            state: StdMutex::new(LinkState::default()),
            dialled: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            stopping: watch::channel(false).0,
        })
    }

    pub fn snapshot(&self) -> Value {
        self.lock_state().view()
    }

    pub fn room_for_devices(&self) -> Option<Value> {
        let pairing = self.current_pairing()?;
        let state = self.lock_state();
        Some(pairing.room_for_devices(state.public_url_for(&pairing)))
    }

    pub fn poke(&self) {
        self.changed.notify_waiters();
    }

    pub fn stop(&self) {
        self.stopping.send_replace(true);
        self.changed.notify_waiters();
    }

    pub async fn run(self: Arc<Self>) {
        let mut stopping = self.stopping.subscribe();
        let mut seen: Option<Pairing> = None;
        let mut delay = FIRST_RETRY;
        loop {
            if *stopping.borrow() {
                break;
            }
            let pairing = self.current_pairing();
            if pairing != seen {
                seen = pairing.clone();
                delay = FIRST_RETRY;
                self.repaired(pairing.as_ref()).await;
            }
            let mut wait = IDLE_POLL;
            if let Some(pairing) = pairing {
                if self.should_dial().await {
                    self.clone().dial(pairing).await;
                    wait = delay;
                    delay = (delay * 2).min(LAST_RETRY);
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {},
                _ = self.changed.notified() => {},
                _ = stopping.changed() => break,
            }
        }
        self.drop_dialled().await;
    }

    pub(super) fn base(&self) -> &Url {
        &self.base
    }

    pub(super) fn current_pairing(&self) -> Option<Pairing> {
        self.pairing_path.as_deref().and_then(Pairing::read)
    }

    /// What this Core proves about itself to the room on either route.
    pub(super) fn identity(&self, pairing: &Pairing) -> Value {
        let mut fields = self
            .room
            .latest_connector_identity()
            .as_object()
            .cloned()
            .unwrap_or_default();
        fields
            .entry("host".to_owned())
            .or_insert_with(|| json!(self.host));
        fields.insert("connector_id".to_owned(), json!(pairing.connector_id));
        fields.insert("token".to_owned(), json!(pairing.token));
        fields.insert("protocol".to_owned(), json!(PROTOCOL));
        fields.insert("core".to_owned(), json!(env!("CARGO_PKG_VERSION")));
        Value::Object(fields)
    }

    pub(super) fn changed(&self) -> Notified<'_> {
        self.changed.notified()
    }

    pub(super) fn stopping(&self) -> watch::Receiver<bool> {
        self.stopping.subscribe()
    }

    pub(super) async fn add_dialled(&self, sid: String, socket: SocketRef) {
        self.dialled.lock().await.insert(sid, socket);
        self.changed.notify_waiters();
    }

    pub(super) async fn remove_dialled(&self, sid: &str) {
        self.dialled.lock().await.remove(sid);
        if self.dialled.lock().await.is_empty() {
            self.disconnected("dial").await;
        }
        self.changed.notify_waiters();
    }

    pub(super) async fn connected(&self, via: &'static str, public_url: Option<String>) {
        let linked = self.current_pairing();
        self.lock_state().connect(via, public_url, linked);
        self.report().await;
    }

    pub(super) async fn disconnected(&self, via: &'static str) {
        let went_down = self.lock_state().disconnect(via);
        if went_down {
            self.report().await;
        }
    }

    pub(super) async fn refused(&self, reason: String) {
        self.lock_state().refuse(&reason);
        self.report().await;
    }

    /// The room revoked this node, with its own reason or the generic one.
    pub(super) async fn revoked(&self, reason: Option<String>) {
        let reason =
            reason.unwrap_or_else(|| render(&LocalizedMessage::new("relay.revoked"), "en"));
        self.refused(reason).await;
    }

    async fn error(&self, key: &'static str) {
        self.lock_state()
            .fail(render(&LocalizedMessage::new(key), "en"));
        self.report().await;
    }

    async fn report(&self) {
        self.room.rendezvous_changed(self.snapshot()).await;
    }

    fn lock_state(&self) -> MutexGuard<'_, LinkState> {
        self.state.lock().expect("rendezvous state lock")
    }

    /// A new pairing drops every route of the previous one.
    async fn repaired(&self, pairing: Option<&Pairing>) {
        self.drop_dialled().await;
        self.lock_state()
            .repaired(pairing.map(|p| p.origin.clone()));
        self.report().await;
    }

    async fn should_dial(&self) -> bool {
        let dialled = !self.dialled.lock().await.is_empty();
        !dialled && !self.lock_state().is_refused()
    }

    /// One outbound attempt. The caller retries with backoff.
    async fn dial(self: Arc<Self>, pairing: Pairing) {
        if !crate::server::safe_url(&pairing.origin) {
            self.error("relay.credential_transport").await;
        } else if client::run(self.clone(), pairing).await.is_err() {
            self.error("relay.node_unavailable").await;
        }
    }

    async fn drop_dialled(&self) {
        let sockets = std::mem::take(&mut *self.dialled.lock().await);
        for (_, socket) in sockets {
            let _ = socket.disconnect();
        }
    }
}
