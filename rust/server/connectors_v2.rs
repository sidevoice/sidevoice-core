//! The pinned connector's Socket.IO v2 event link on the Unix listener.
use super::AppState;
use crate::control::room::{ConnectorPeer, PeerError, PeerRequest};
use axum::Router;
use serde_json::{json, Value};
use socketioxide::{
    extract::{AckSender, SocketRef, State, TryData},
    SocketIo,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;
fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

pub fn layer(app: Router, state: Arc<AppState>) -> Router {
    let (layer, io) = SocketIo::builder()
        .req_path("/api/connectors/link")
        .ping_interval(Duration::from_secs(15))
        .ping_timeout(Duration::from_secs(30))
        .ack_timeout(Duration::from_secs(60))
        .with_state(state)
        .build_layer();
    io.ns("/connectors", connect);
    app.layer(layer)
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
    let token = field(&auth, "token");
    if auth.get("protocol").and_then(Value::as_i64) != Some(2)
        || !state.room.authenticate_connector(cid, token, &auth)
    {
        let _ = socket.disconnect();
        return;
    }
    let cid = cid.to_owned();
    let generation = Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::channel::<PeerRequest>(128);
    let (stop, mut stopped) = watch::channel(false);
    let peer = ConnectorPeer {
        generation: generation.clone(),
        sender: tx,
        stop,
    };
    let old = state.room.attach(&cid, peer);
    if let Some(old) = old {
        old.disconnect();
    }
    let socket_to_close = socket.clone();
    tokio::spawn(async move {
        if stopped.changed().await.is_ok() {
            let _ = socket_to_close.disconnect();
        }
    });
    let _ = socket.emit("connector.welcome", &json!({"protocol":2}));
    let dispatch_socket = socket.clone();
    tokio::spawn(async move {
        while let Some(PeerRequest {
            method,
            params,
            answer,
        }) = rx.recv().await
        {
            if let Some(answer) = answer {
                let dispatch_socket = dispatch_socket.clone();
                tokio::spawn(async move {
                    let outcome = dispatch_socket
                        .timeout(Duration::from_secs(60))
                        .emit_with_ack::<_, Value>(&method, &params);
                    let result = match outcome {
                        Ok(stream) => stream.await.map_err(|_| PeerError),
                        Err(_) => Err(PeerError),
                    };
                    let _ = answer.send(result);
                });
            } else {
                let _ = dispatch_socket.emit(&method, &params);
            }
        }
    });
    let disconnected = state.clone();
    let gone_cid = cid.clone();
    let gone_gen = generation.clone();
    socket.on_disconnect(move |_: SocketRef| {
        let state = disconnected.clone();
        let cid = gone_cid.clone();
        let generation = gone_gen.clone();
        async move {
            state.room.detach(&cid, &generation);
        }
    });
    let room = state.room.clone();
    let cid1 = cid.clone();
    let gen = generation.clone();
    socket.on(
        "binding.register",
        move |TryData(data): TryData<Value>, ack: AckSender| {
            let room = room.clone();
            let cid = cid1.clone();
            let gen = gen.clone();
            async move {
                let answer = if room.current_peer(&cid, &gen) {
                    room.register(&cid, &data.unwrap_or(json!({})))
                        .unwrap_or_else(|e| json!({"error":e.key}))
                } else {
                    json!({"error":"room.connector_disconnected"})
                };
                let _ = ack.send(&answer);
            }
        },
    );
    let room = state.room.clone();
    let cid1 = cid.clone();
    let gen = generation.clone();
    socket.on(
        "binding.unregister",
        move |TryData(data): TryData<Value>| {
            let room = room.clone();
            let cid = cid1.clone();
            let gen = gen.clone();
            async move {
                if room.current_peer(&cid, &gen) {
                    room.unregister(&cid, field(&data.unwrap_or(json!({})), "binding_id"));
                }
            }
        },
    );
    let room = state.room.clone();
    let cid1 = cid.clone();
    let gen = generation.clone();
    socket.on(
        "speech.publish",
        move |TryData(data): TryData<Value>, ack: AckSender| {
            let room = room.clone();
            let cid = cid1.clone();
            let gen = gen.clone();
            async move {
                let data = data.unwrap_or(json!({}));
                let mut answer = if room.current_peer(&cid, &gen) {
                    room.connector_speech(&cid, &data, false)
                } else {
                    json!({"status":"rejected","error":"room.connector_disconnected"})
                };
                if let Some(obj) = answer.as_object_mut() {
                    obj.insert(
                        "event_id".into(),
                        data.get("event_id").cloned().unwrap_or(Value::Null),
                    );
                }
                let _ = ack.send(&answer);
            }
        },
    );
    let room = state.room.clone();
    let cid1 = cid.clone();
    let gen = generation.clone();
    socket.on("input.working", move |TryData(data): TryData<Value>| {
        let room = room.clone();
        let cid = cid1.clone();
        let gen = gen.clone();
        async move {
            if room.current_peer(&cid, &gen) {
                room.working(&cid, &data.unwrap_or(json!({})));
            }
        }
    });
    let room = state.room.clone();
    let cid1 = cid.clone();
    let gen = generation.clone();
    socket.on("input.engine", move |TryData(data): TryData<Value>| {
        let room = room.clone();
        let cid = cid1.clone();
        let gen = gen.clone();
        async move {
            if room.current_peer(&cid, &gen) {
                room.engine(&cid, &data.unwrap_or(json!({})));
            }
        }
    });
    let room = state.room.clone();
    let cid1 = cid.clone();
    let gen = generation.clone();
    socket.on("input.read", move |TryData(data): TryData<Value>| {
        let room = room.clone();
        let cid = cid1.clone();
        let gen = gen.clone();
        async move {
            if room.current_peer(&cid, &gen) {
                room.read(&cid, &data.unwrap_or(json!({})));
            }
        }
    });
    let cid1 = cid.clone();
    let gen = generation.clone();
    socket.on(
        "device.pairing_code",
        move |ack: AckSender, State(state): State<Arc<AppState>>| {
            let cid = cid1.clone();
            let gen = gen.clone();
            async move {
                if state.room.current_peer(&cid, &gen) {
                    let _ = ack.send(&state.issue_code());
                }
            }
        },
    );
}
