//! The room's inbound dial on `/room`: authenticate it, greet the room, and
//! serve its relay events on the dialled socket.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use serde_json::Value;
use socketioxide::{
    extract::{AckSender, SocketRef, State, TryData},
    handler::ConnectHandler,
    SocketIo,
};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::link::Rendezvous;
use super::packet::{from_rmpv, to_rmpv, Part};
use super::relay::Relay;
use crate::messages::{render, LocalizedMessage};
use crate::server::AppState;

#[cfg(test)]
mod tests;

const DIAL_PATH: &str = "/api/rendezvous/link";
const DIAL_NAMESPACE: &str = "/room";
const RELAY_EVENTS: [&str; 4] = ["relay.http", "relay.open", "relay.data", "relay.close"];

pub fn layer(app: Router, state: Arc<AppState>) -> Router {
    let (layer, io) = SocketIo::builder()
        .req_path(DIAL_PATH)
        .with_state(state)
        .build_layer();
    io.ns(DIAL_NAMESPACE, dial_connect.with(authenticate));
    app.layer(layer)
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
    rv.add_dialled(sid.clone(), socket.clone()).await;
    let (outbound, output) = mpsc::channel::<(&'static str, Part)>(128);
    let relay = Arc::new(Relay::new(rv.base().clone(), outbound));
    disconnect_when_relay_stops(&socket, &relay);
    let output_abort = forward_output(&socket, output);
    serve_relay_events(&socket, &relay);
    on_revoked(&socket, rv.clone());
    on_disconnect(&socket, rv.clone(), relay, sid, output_abort);
    greet(socket, rv.clone(), rv.identity(&pairing));
}

fn disconnect_when_relay_stops(socket: &SocketRef, relay: &Relay) {
    let mut relay_stopped = relay.stopped();
    let failed_socket = socket.clone();
    tokio::spawn(async move {
        if relay_stopped.changed().await.is_ok() {
            let _ = failed_socket.disconnect();
        }
    });
}

fn forward_output(
    socket: &SocketRef,
    mut output: mpsc::Receiver<(&'static str, Part)>,
) -> AbortHandle {
    let emitted = socket.clone();
    let output_task = tokio::spawn(async move {
        while let Some((event, part)) = output.recv().await {
            if emitted.emit(event, &to_rmpv(part)).is_err() {
                let _ = emitted.disconnect();
                break;
            }
        }
    });
    output_task.abort_handle()
}

fn serve_relay_events(socket: &SocketRef, relay: &Arc<Relay>) {
    for event in RELAY_EVENTS {
        let relay = relay.clone();
        socket.on(
            event,
            move |TryData(data): TryData<rmpv::Value>, ack: AckSender| {
                let relay = relay.clone();
                async move {
                    let Some(data) = data.ok().and_then(from_rmpv) else {
                        return;
                    };
                    if let Some(answer) = relay.handle(event, data).await {
                        let _ = ack.send(&to_rmpv(answer));
                    }
                }
            },
        );
    }
}

fn on_revoked(socket: &SocketRef, rv: Arc<Rendezvous>) {
    socket.on(
        "node.revoked",
        move |TryData(data): TryData<Value>, socket: SocketRef| {
            let rv = rv.clone();
            async move {
                let reason = data
                    .ok()
                    .and_then(|d| d.get("reason").and_then(Value::as_str).map(str::to_owned));
                rv.revoked(reason).await;
                let _ = socket.disconnect();
            }
        },
    );
}

fn on_disconnect(
    socket: &SocketRef,
    rv: Arc<Rendezvous>,
    relay: Arc<Relay>,
    sid: String,
    output_abort: AbortHandle,
) {
    socket.on_disconnect(move |_: SocketRef| {
        let rv = rv.clone();
        let relay = relay.clone();
        let sid = sid.clone();
        output_abort.abort();
        async move {
            relay.shutdown().await;
            rv.remove_dialled(&sid).await;
        }
    });
}

/// Send this Core's identity and keep the socket only if the room accepts it.
fn greet(socket: SocketRef, rv: Arc<Rendezvous>, proof: Value) {
    tokio::spawn(async move {
        let answer = socket
            .timeout(Duration::from_secs(10))
            .emit_with_ack::<_, Value>("node.hello", &proof);
        match answer {
            Ok(answer) => match answer.await {
                Ok(answer) if valid_hello(&answer) => rv.connected("dial", None).await,
                Ok(answer) => {
                    rv.refused(hello_refusal(&answer)).await;
                    let _ = socket.disconnect();
                }
                Err(_) => {
                    let _ = socket.disconnect();
                }
            },
            Err(_) => {
                let _ = socket.disconnect();
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
