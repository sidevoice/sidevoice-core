//! One accepted v3 connection: requests flow both ways over the socket until the
//! connector leaves or the room replaces it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use crate::control::room::{PeerError, PeerRequest};
use crate::server::connectors::{field, Attached, Link};
use crate::server::AppState;

use super::dispatch::dispatch;
use super::frame::{self, close, decode, error, text};

const OUTGOING_QUEUE: usize = 128;
/// Node requests awaiting the connector's answer.
const MAX_PENDING: usize = 128;
/// Connector requests the node runs at once.
const MAX_INCOMING: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

type Answer = oneshot::Sender<Result<Value, PeerError>>;

enum Flow {
    Continue,
    Stop,
}

pub(super) struct Connection {
    state: Arc<AppState>,
    socket: WebSocket,
    link: Link,
    requests: mpsc::Receiver<PeerRequest>,
    stopped: watch::Receiver<bool>,
    /// Frames for the socket, from this loop and from dispatched requests.
    out: mpsc::Sender<Value>,
    outgoing: mpsc::Receiver<Value>,
    /// The node's requests by their `s:` id, until the connector answers.
    pending: HashMap<String, Answer>,
    /// Ids of the connector's requests still being dispatched.
    incoming: HashSet<String>,
    finished: mpsc::Sender<String>,
    completed: mpsc::Receiver<String>,
    seq: u64,
}

impl Connection {
    /// Answers the hello and greets the connector as the room's peer.
    pub(super) async fn open(
        state: Arc<AppState>,
        socket: WebSocket,
        attached: Attached,
        hello_id: Value,
    ) -> Self {
        let Attached {
            link,
            requests,
            stopped,
        } = attached;
        let (out, outgoing) = mpsc::channel::<Value>(OUTGOING_QUEUE);
        let _ = out
            .send(json!({"jsonrpc":"2.0","id":hello_id,"result":{"protocol":3}}))
            .await;
        let _ = out
            .send(json!({"jsonrpc":"2.0","method":"connector.welcome","params":{"protocol":3}}))
            .await;
        let _ = out
            .send(json!({"jsonrpc":"2.0","method":"node.rendezvous","params":state.rendezvous.snapshot()}))
            .await;
        let (finished, completed) = mpsc::channel::<String>(MAX_INCOMING);
        Self {
            state,
            socket,
            link,
            requests,
            stopped,
            out,
            outgoing,
            pending: HashMap::new(),
            incoming: HashSet::new(),
            finished,
            completed,
            seq: 0,
        }
    }

    pub(super) async fn serve(mut self) {
        loop {
            let flow = tokio::select! {
                _ = self.stopped.changed() => {
                    let _ = self.socket.send(close(frame::NORMAL)).await;
                    Flow::Stop
                }
                done = self.completed.recv() => {
                    if let Some(key) = done {
                        self.incoming.remove(&key);
                    }
                    Flow::Continue
                }
                command = self.requests.recv() => match command {
                    Some(command) => self.forward(command).await,
                    None => Flow::Stop,
                },
                outgoing = self.outgoing.recv() => match outgoing {
                    Some(outgoing) => self.write(outgoing).await,
                    None => Flow::Stop,
                },
                incoming = self.socket.recv() => match incoming {
                    Some(Ok(incoming)) => self.read(incoming).await,
                    _ => Flow::Stop,
                },
            };
            if matches!(flow, Flow::Stop) {
                break;
            }
        }
        self.link.detach(&self.state.room);
        for (_, answer) in self.pending {
            let _ = answer.send(Err(PeerError));
        }
    }

    /// Sends a room request or notification to the connector.
    async fn forward(&mut self, command: PeerRequest) -> Flow {
        let PeerRequest {
            method,
            params,
            answer,
        } = command;
        let Some(answer) = answer else {
            return self
                .queue(json!({"jsonrpc":"2.0","method":method,"params":params}))
                .await;
        };
        self.pending.retain(|_, sender| !sender.is_closed());
        if self.pending.len() >= MAX_PENDING {
            let _ = answer.send(Err(PeerError));
            return Flow::Continue;
        }
        self.seq += 1;
        let request_id = format!("s:{}", self.seq);
        self.pending.insert(request_id.clone(), answer);
        self.queue(json!({"jsonrpc":"2.0","id":request_id,"method":method,"params":params}))
            .await
    }

    // `&mut` keeps the future `Send`: the socket is not `Sync`.
    async fn queue(&mut self, outgoing: Value) -> Flow {
        if self.out.send(outgoing).await.is_err() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    async fn write(&mut self, outgoing: Value) -> Flow {
        let Some(message) = text(outgoing) else {
            let _ = self.socket.send(close(frame::TOO_BIG)).await;
            return Flow::Stop;
        };
        if self.socket.send(message).await.is_err() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    async fn read(&mut self, message: Message) -> Flow {
        if !self.link.current(&self.state.room) {
            let _ = self.socket.send(close(frame::NORMAL)).await;
            return Flow::Stop;
        }
        let raw = match message {
            Message::Text(raw) => raw,
            Message::Binary(_) => {
                let _ = self.socket.send(close(frame::UNSUPPORTED_DATA)).await;
                return Flow::Stop;
            }
            _ => return Flow::Continue,
        };
        let incoming = match decode(&raw) {
            Ok(incoming) => incoming,
            Err(code) => {
                let _ = self.socket.send(close(code)).await;
                return Flow::Stop;
            }
        };
        if incoming.get("method").is_none() {
            self.settle(&incoming);
            return Flow::Continue;
        }
        self.accept(incoming).await
    }

    /// Resolves the node request this response answers, if it is still pending.
    fn settle(&mut self, response: &Value) {
        let key = response["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| response["id"].to_string());
        if let Some(answer) = self.pending.remove(&key) {
            let outcome = if response.get("error").is_some() {
                Err(PeerError)
            } else {
                Ok(response.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = answer.send(outcome);
        }
    }

    /// Dispatches a connector request or notification off the loop, within capacity.
    async fn accept(&mut self, request: Value) -> Flow {
        let request_id = request.get("id").cloned();
        let key = request_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        if request_id.is_some() && !self.incoming.insert(key.clone()) {
            let _ = self.socket.send(close(frame::PROTOCOL_ERROR)).await;
            return Flow::Stop;
        }
        if self.incoming.len() > MAX_INCOMING {
            if let Some(id) = request_id {
                let _ = self
                    .out
                    .send(error(id, -32000, "Connector request capacity reached"))
                    .await;
                self.incoming.remove(&key);
            }
            return Flow::Continue;
        }
        let method = field(&request, "method").to_owned();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let out = self.out.clone();
        let finished = self.finished.clone();
        let state = self.state.clone();
        let link = self.link.clone();
        tokio::spawn(async move {
            let outcome =
                tokio::time::timeout(REQUEST_TIMEOUT, dispatch(state, link, method, params)).await;
            let Some(id) = request_id else {
                return;
            };
            let reply = match outcome {
                Ok(Ok(result)) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                Ok(Err((code, message))) => error(id, code, message),
                Err(_) => error(id, -32001, "Request timed out"),
            };
            let _ = out.send(reply).await;
            let _ = finished.send(key).await;
        });
        Flow::Continue
    }
}
