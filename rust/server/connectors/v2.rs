//! The pinned connector's Socket.IO v2 event link on the Unix listener.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use serde_json::{json, Value};
use socketioxide::{
    extract::{SocketRef, State, TryData},
    handler::ConnectHandler,
    SocketIo,
};
use tokio::sync::{mpsc, watch};

use crate::control::room::{PeerError, PeerRequest};
use crate::messages::{render, LocalizedMessage};
use crate::server::AppState;

use super::{attach, field, Attached};

mod events;
#[cfg(test)]
mod tests;

const ACK_TIMEOUT: Duration = Duration::from_secs(60);

pub(in crate::server) fn layer(app: Router, state: Arc<AppState>) -> Router {
    let (layer, io) = SocketIo::builder()
        .req_path("/api/connectors/link")
        .ping_interval(Duration::from_secs(15))
        .ping_timeout(Duration::from_secs(30))
        .ack_timeout(ACK_TIMEOUT)
        .with_state(state)
        .build_layer();
    io.ns("/connectors", connect.with(admit));
    app.layer(layer)
}

/// Why a v2 connection is refused, said in its `connect_error`, or None when it is admitted. The credential is checked
/// first and then the protocol, as the Python core did, so a wrong token never learns which protocol is spoken here.
fn refusal(
    auth: Option<&Value>,
    authenticate: impl FnOnce(&str, &str, &Value) -> bool,
) -> Option<String> {
    let refused = || render(&LocalizedMessage::new("connector.credential_refused"), "en");
    let Some(auth) = auth.filter(|auth| auth.is_object()) else {
        return Some(refused());
    };
    if !authenticate(field(auth, "connector_id"), field(auth, "token"), auth) {
        return Some(refused());
    }
    let protocol = auth.get("protocol").cloned().unwrap_or(Value::Null);
    (protocol.as_i64() != Some(2)).then(|| {
        render(
            &LocalizedMessage::new("connector.protocol_unsupported")
                .with_param("protocol", protocol.to_string())
                .with_param("supported", 2),
            "en",
        )
    })
}

/// Runs before the namespace connects, so a refusal reaches the connector as `connect_error` with its reason
/// instead of a bare disconnect.
async fn admit(
    State(state): State<Arc<AppState>>,
    TryData(auth): TryData<Value>,
) -> Result<(), String> {
    match refusal(auth.as_ref().ok(), |cid, token, auth| {
        state.room.authenticate_connector(cid, token, auth)
    }) {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

async fn connect(
    socket: SocketRef,
    State(state): State<Arc<AppState>>,
    TryData(auth): TryData<Value>,
) {
    let Ok(auth) = auth else {
        let _ = socket.disconnect();
        return;
    };
    let cid = field(&auth, "connector_id");
    let Attached {
        link,
        requests,
        stopped,
    } = attach(&state.room, cid.to_owned());
    disconnect_when_stopped(socket.clone(), stopped);
    let _ = socket.emit("connector.welcome", &json!({"protocol":2}));
    let _ = socket.emit("node.rendezvous", &state.rendezvous.snapshot());
    forward_requests(socket.clone(), requests);
    let room = state.room.clone();
    let gone = link.clone();
    socket.on_disconnect(move |_: SocketRef| {
        let room = room.clone();
        let gone = gone.clone();
        async move {
            gone.detach(&room);
        }
    });
    events::register(&socket, &state.room, &link);
}

fn disconnect_when_stopped(socket: SocketRef, mut stopped: watch::Receiver<bool>) {
    tokio::spawn(async move {
        if stopped.changed().await.is_ok() {
            let _ = socket.disconnect();
        }
    });
}

/// Emits the room's requests to the connector, waiting for acks where asked.
fn forward_requests(socket: SocketRef, mut requests: mpsc::Receiver<PeerRequest>) {
    tokio::spawn(async move {
        while let Some(PeerRequest {
            method,
            params,
            answer,
        }) = requests.recv().await
        {
            let Some(answer) = answer else {
                let _ = socket.emit(&method, &params);
                continue;
            };
            let socket = socket.clone();
            tokio::spawn(async move {
                let outcome = socket
                    .timeout(ACK_TIMEOUT)
                    .emit_with_ack::<_, Value>(&method, &params);
                let result = match outcome {
                    Ok(stream) => stream.await.map_err(|_| PeerError),
                    Err(_) => Err(PeerError),
                };
                let _ = answer.send(result);
            });
        }
    });
}
