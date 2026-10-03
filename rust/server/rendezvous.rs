//! Existing room rendezvous link, with Socket.IO and loopback forwarding.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::Router;
use percent_encoding::percent_decode_str;
use serde_json::{json, Value};
use socketioxide::{
    extract::{AckSender, SocketRef, State, TryData},
    handler::ConnectHandler,
    SocketIo,
};
use tokio::sync::{mpsc, watch, Mutex, Notify};
use url::Url;

use crate::control::room::Room;
use crate::messages::{render, LocalizedMessage};

use super::AppState;

pub(crate) const PROTOCOL: u8 = 3;
pub(crate) const OUTBOUND_PATH: &str = "/api/connectors/link";
pub(crate) const OUTBOUND_NAMESPACE: &str = "/nodes";
pub(crate) const DIAL_PATH: &str = "/api/rendezvous/link";
pub(crate) const DIAL_NAMESPACE: &str = "/room";
pub(crate) const CALL_SOCKET: &str = "/api/presentation/ws";

mod client;
mod packet;
mod relay;

use packet::{from_rmpv, to_rmpv, Part};
use relay::Relay;

struct LinkState {
    room: Option<String>,
    connected: bool,
    via: Option<&'static str>,
    error: Option<String>,
    refused: Option<String>,
    public_url: Option<String>,
    linked: Option<Pairing>,
}

impl LinkState {
    fn view(&self) -> Value {
        json!({"room":self.room,"connected":self.connected,"via":self.via,
            "error":self.error,"refused":self.refused})
    }
}

pub struct Rendezvous {
    pairing_path: Option<PathBuf>,
    base: Url,
    host: String,
    room: Arc<Room>,
    state: StdMutex<LinkState>,
    dialled: Mutex<std::collections::HashMap<String, SocketRef>>,
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
            state: StdMutex::new(LinkState {
                room: None,
                connected: false,
                via: None,
                error: None,
                refused: None,
                public_url: None,
                linked: None,
            }),
            dialled: Mutex::new(std::collections::HashMap::new()),
            changed: Notify::new(),
            stopping: watch::channel(false).0,
        })
    }

    fn current_pairing(&self) -> Option<Pairing> {
        self.pairing_path.as_deref().and_then(Pairing::read)
    }

    fn identity(&self, pairing: &Pairing) -> Value {
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

    pub fn snapshot(&self) -> Value {
        self.state.lock().expect("rendezvous state lock").view()
    }

    pub fn room_for_devices(&self) -> Option<Value> {
        let pairing = self.current_pairing()?;
        let state = self.state.lock().expect("rendezvous state lock");
        let public = (state.linked.as_ref() == Some(&pairing))
            .then(|| state.public_url.as_deref())
            .flatten();
        Some(pairing.room_for_devices(public))
    }

    async fn report(&self) {
        self.room.rendezvous_changed(self.snapshot()).await;
    }

    async fn connected(&self, via: &'static str, public_url: Option<String>) {
        {
            let mut state = self.state.lock().expect("rendezvous state lock");
            state.connected = true;
            state.via = Some(via);
            state.error = None;
            state.refused = None;
            state.public_url = public_url;
            state.linked = self.current_pairing();
        }
        self.report().await;
    }

    async fn disconnected(&self, via: &'static str) {
        {
            let mut state = self.state.lock().expect("rendezvous state lock");
            if state.via != Some(via) {
                return;
            }
            state.connected = false;
            state.via = None;
            state.public_url = None;
            state.linked = None;
        }
        self.report().await;
    }

    async fn refused(&self, reason: String) {
        {
            let mut state = self.state.lock().expect("rendezvous state lock");
            state.connected = false;
            state.refused = Some(reason.chars().take(200).collect());
            state.error = None;
            state.via = None;
            state.public_url = None;
            state.linked = None;
        }
        self.report().await;
    }

    async fn error(&self, key: &'static str) {
        {
            let mut state = self.state.lock().expect("rendezvous state lock");
            state.error = Some(render(&LocalizedMessage::new(key), "en"));
            state.connected = false;
            state.via = None;
        }
        self.report().await;
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
        let mut delay = Duration::from_millis(250);
        loop {
            if *stopping.borrow() {
                break;
            }
            let pairing = self.current_pairing();
            if pairing != seen {
                seen = pairing.clone();
                delay = Duration::from_millis(250);
                let sockets = std::mem::take(&mut *self.dialled.lock().await);
                for (_, socket) in sockets {
                    let _ = socket.disconnect();
                }
                {
                    let mut state = self.state.lock().expect("rendezvous state lock");
                    state.room = pairing.as_ref().map(|p| p.origin.clone());
                    state.connected = false;
                    state.via = None;
                    state.error = None;
                    state.refused = None;
                    state.public_url = None;
                    state.linked = None;
                }
                self.report().await;
            }
            if let Some(pairing) = pairing {
                let dialled = !self.dialled.lock().await.is_empty();
                let refused = self
                    .state
                    .lock()
                    .expect("rendezvous state lock")
                    .refused
                    .is_some();
                if !dialled && !refused {
                    if !super::safe_url(&pairing.origin) {
                        self.error("relay.credential_transport").await;
                    } else if client::run(self.clone(), pairing).await.is_err() {
                        self.error("relay.node_unavailable").await;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {},
                        _ = self.changed.notified() => {},
                        _ = stopping.changed() => break,
                    }
                    delay = (delay * 2).min(Duration::from_secs(10));
                    continue;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                _ = self.changed.notified() => {},
                _ = stopping.changed() => break,
            }
        }
        let sockets = std::mem::take(&mut *self.dialled.lock().await);
        for (_, socket) in sockets {
            let _ = socket.disconnect();
        }
    }
}

fn field<'a>(data: &'a Value, name: &str) -> &'a str {
    data.get(name).and_then(Value::as_str).unwrap_or("")
}

async fn authenticate(
    State(state): State<Arc<AppState>>,
    TryData(auth): TryData<Value>,
) -> Result<(), String> {
    let pairing = state.rendezvous.current_pairing();
    let accepted = pairing
        .as_ref()
        .zip(auth.ok())
        .is_some_and(|(pairing, auth)| {
            let told_key = field(&auth, "dial_key");
            field(&auth, "connector_id") == pairing.connector_id
                && pairing.dial_key.as_deref().is_some_and(|key| {
                    use subtle::ConstantTimeEq;
                    key.as_bytes().ct_eq(told_key.as_bytes()).into()
                })
        });
    if accepted {
        Ok(())
    } else {
        Err(render(&LocalizedMessage::new("relay.dial_refused"), "en"))
    }
}

async fn dial_connect(socket: SocketRef, State(state): State<Arc<AppState>>) {
    let rv = state.rendezvous.clone();
    let Some(pairing) = rv.current_pairing() else {
        let _ = socket.disconnect();
        return;
    };
    let sid = socket.id.to_string();
    rv.dialled.lock().await.insert(sid.clone(), socket.clone());
    rv.changed.notify_waiters();
    let (outbound, mut output) = mpsc::channel::<(&'static str, Part)>(128);
    let relay = Arc::new(Relay::new(rv.base.clone(), outbound));
    let mut relay_stopped = relay.stopped();
    let failed_socket = socket.clone();
    tokio::spawn(async move {
        if relay_stopped.changed().await.is_ok() {
            let _ = failed_socket.disconnect();
        }
    });
    let emitted = socket.clone();
    let output_task = tokio::spawn(async move {
        while let Some((event, part)) = output.recv().await {
            if emitted.emit(event, &to_rmpv(part)).is_err() {
                break;
            }
        }
    });
    let output_abort = output_task.abort_handle();
    for event in ["relay.http", "relay.open", "relay.data", "relay.close"] {
        let relay = relay.clone();
        socket.on(
            event,
            move |TryData(data): TryData<rmpv::Value>, ack: AckSender| {
                let relay = relay.clone();
                async move {
                    if let Ok(data) = data {
                        if let Some(data) = from_rmpv(data) {
                            if let Some(answer) = relay.handle(event, data).await {
                                let _ = ack.send(&to_rmpv(answer));
                            }
                        }
                    }
                }
            },
        );
    }
    let revoked = rv.clone();
    socket.on(
        "node.revoked",
        move |TryData(data): TryData<Value>, socket: SocketRef| {
            let rv = revoked.clone();
            async move {
                let reason = data
                    .ok()
                    .and_then(|d| d.get("reason").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_else(|| render(&LocalizedMessage::new("relay.revoked"), "en"));
                rv.refused(reason).await;
                let _ = socket.disconnect();
            }
        },
    );
    let disconnected = rv.clone();
    let gone_relay = relay.clone();
    socket.on_disconnect(move |_: SocketRef| {
        let rv = disconnected.clone();
        let relay = gone_relay.clone();
        let sid = sid.clone();
        output_abort.abort();
        async move {
            relay.shutdown().await;
            rv.dialled.lock().await.remove(&sid);
            if rv.dialled.lock().await.is_empty() {
                rv.disconnected("dial").await;
            }
            rv.changed.notify_waiters();
        }
    });
    let proof = rv.identity(&pairing);
    let socket_greet = socket.clone();
    tokio::spawn(async move {
        let answer = socket_greet
            .timeout(Duration::from_secs(10))
            .emit_with_ack::<_, Value>("node.hello", &proof);
        match answer {
            Ok(answer) => match answer.await {
                Ok(answer) if valid_hello(&answer) => rv.connected("dial", None).await,
                Ok(answer) => {
                    rv.refused(hello_refusal(&answer)).await;
                    let _ = socket_greet.disconnect();
                }
                Err(_) => {
                    let _ = socket_greet.disconnect();
                }
            },
            Err(_) => {
                let _ = socket_greet.disconnect();
            }
        }
    });
}

fn valid_hello(answer: &Value) -> bool {
    answer.is_object() && !answer.get("error").is_some_and(|value| !value.is_null())
}

fn hello_refusal(answer: &Value) -> String {
    answer
        .get("error")
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| render(&LocalizedMessage::new("relay.hello_refused"), "en"))
}

pub fn layer(app: Router, state: Arc<AppState>) -> Router {
    let (layer, io) = SocketIo::builder()
        .req_path(DIAL_PATH)
        .with_state(state)
        .build_layer();
    io.ns(DIAL_NAMESPACE, dial_connect.with(authenticate));
    app.layer(layer)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pairing {
    pub url: String,
    pub origin: String,
    pub connector_id: String,
    pub token: String,
    pub dial_key: Option<String>,
}

impl Pairing {
    /// Read only the connector-owned file. A missing or malformed pairing is
    /// indistinguishable from an unpaired machine to the watcher.
    pub(crate) fn read(path: &Path) -> Option<Self> {
        let saved: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let field = |name| {
            saved
                .get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let url = field("url")?.to_owned();
        let parsed = Url::parse(&url).ok()?;
        let host = parsed.host_str()?;
        let scheme = match parsed.scheme() {
            "ws" | "http" => "http",
            "wss" | "https" => "https",
            _ => return None,
        };
        let authority_host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let mut origin = format!("{scheme}://{authority_host}");
        if let Some(port) = parsed.port() {
            origin.push_str(&format!(":{port}"));
        }
        Some(Self {
            url,
            origin,
            connector_id: field("connector_id")?.to_owned(),
            token: field("token")?.to_owned(),
            dial_key: field("dial_key").map(str::to_owned),
        })
    }

    pub(crate) fn room_for_devices(&self, public_url: Option<&str>) -> Value {
        serde_json::json!({"url": public_url.unwrap_or(&self.origin), "node": self.connector_id})
    }
}

pub(crate) fn public_origin(value: &str) -> Option<String> {
    if value.len() > 2048 {
        return None;
    }
    let trimmed = value.trim().trim_end_matches('/');
    let url = Url::parse(trimmed).ok()?;
    matches!(url.scheme(), "http" | "https")
        .then(|| url.host_str())
        .flatten()
        .map(|_| trimmed.to_owned())
}

fn relayed(path: &str, local_only: impl Fn(&str) -> bool) -> bool {
    [
        "/api/presentation",
        "/api/device",
        "/api/models",
        "/api/host",
    ]
    .iter()
    .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
        && !local_only(path)
}

/// Check both every percent-decoded spelling and the path `url::Url` will
/// actually request. The caller supplies T1's local-only rule, so there is one
/// owner for the TCP/UDS exclusion policy.
pub(crate) fn relayable(base: &Url, path: &str, local_only: impl Fn(&str) -> bool) -> bool {
    if !path.starts_with('/') || path.starts_with("//") || path.contains('?') || path.contains('#')
    {
        return false;
    }
    let mut current = path.to_owned();
    for _ in 0..=path.len() {
        if !relayed(&current, &local_only) || current.contains("..") || current.contains('\\') {
            return false;
        }
        let next = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if next == current {
            let Ok(final_url) = base.join(path) else {
                return false;
            };
            return final_url.origin() == base.origin() && relayed(final_url.path(), local_only);
        }
        current = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_and_public_origin_keep_existing_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        fs::write(&path, r#"{"url":"wss://room.example:444/link","connector_id":"node-1","token":"secret","dial_key":"proof"}"#).unwrap();
        let pairing = Pairing::read(&path).unwrap();
        assert_eq!(pairing.origin, "https://room.example:444");
        assert_eq!(pairing.dial_key.as_deref(), Some("proof"));
        assert_eq!(
            pairing.room_for_devices(None),
            serde_json::json!({"url":"https://room.example:444","node":"node-1"})
        );
        assert_eq!(
            public_origin(" https://room.example/ "),
            Some("https://room.example".into())
        );
        fs::write(
            &path,
            r#"{"url":"file:///tmp/key","connector_id":"node-1","token":"secret"}"#,
        )
        .unwrap();
        assert!(Pairing::read(&path).is_none());
        fs::write(
            &path,
            r#"{"url":"ws://[::1]:8768/link","connector_id":"node-1","token":"secret"}"#,
        )
        .unwrap();
        assert_eq!(Pairing::read(&path).unwrap().origin, "http://[::1]:8768");
    }

    #[test]
    fn dial_hello_requires_an_object_without_error() {
        assert!(valid_hello(&serde_json::json!({"protocol": 3})));
        assert!(valid_hello(&serde_json::json!({"error": null})));
        for answer in [
            serde_json::Value::Null,
            serde_json::json!("bad"),
            serde_json::json!([]),
            serde_json::json!({"error":"rejected"}),
        ] {
            assert!(!valid_hello(&answer));
        }
    }

    #[tokio::test]
    async fn malformed_hello_reports_a_connector_visible_refusal() {
        use crate::control::room::ConnectorPeer;
        use crate::storage::PrivateDir;

        let temp = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open(temp.path().join("core")).unwrap();
        let room = Arc::new(Room::load(dir).unwrap());
        let (sender, mut receiver) = mpsc::channel(2);
        let (stop, _) = watch::channel(false);
        room.attach(
            "fixture",
            ConnectorPeer {
                generation: "one".into(),
                sender,
                stop,
            },
        );
        let rv = Rendezvous::new(
            None,
            Url::parse("http://127.0.0.1:8768/").unwrap(),
            "fixture".into(),
            room,
        );
        rv.refused(hello_refusal(&serde_json::json!("malformed")))
            .await;
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.method, "node.rendezvous");
        assert_eq!(event.params["connected"], false);
        let reason = event.params["refused"].as_str().unwrap();
        assert!(
            !reason.is_empty(),
            "Connector's truthy refusal branch must run"
        );
        assert_eq!(
            reason,
            render(&LocalizedMessage::new("relay.hello_refused"), "en")
        );
        assert_eq!(rv.snapshot()["refused"].as_str(), Some(reason));
        assert_eq!(
            hello_refusal(&serde_json::json!({"error": "specific"})),
            "specific"
        );
    }

    #[test]
    fn relay_path_never_crosses_local_or_rendezvous_boundary() {
        let base = Url::parse("http://127.0.0.1:8768/").unwrap();
        let local_only = |path: &str| path.starts_with("/api/device/local");
        for path in [
            "/api/presentation/ws",
            "/api/models/catalog",
            "/api/host/agents",
            "/api/device/identity",
        ] {
            assert!(relayable(&base, path, local_only), "{path}");
        }
        for path in [
            "/api/connectors/link",
            "/api/rendezvous",
            "/api/device/local/pair",
            "/api/device/%2e%2e/connectors/link",
            "/api/presentation/%252e%252e/connectors",
            "/api/models/../rendezvous",
            "/api/device/..%2f..%2fapi/connectors",
            "/api/device/%5c..%5cconnectors",
            "//evil.example/api/device",
            "/api/device?next=/api/connectors",
        ] {
            assert!(!relayable(&base, path, local_only), "{path}");
        }
        assert!(!super::super::safe_url("http://room.example"));
    }
}
