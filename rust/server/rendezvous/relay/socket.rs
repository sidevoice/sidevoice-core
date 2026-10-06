//! `relay.open`, `relay.data` and `relay.close`: the call WebSocket, bridged
//! between the room and this Core.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderValue, ORIGIN};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, handshake::client::Request, protocol::CloseFrame, Message,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use super::super::packet::Part;
use super::answer::open_error;
use super::{loopback_origin, query, text, Channel, Relay};

const CALL_SOCKET: &str = "/api/presentation/ws";
const MAX_CHANNELS: usize = 32;
const MAX_PROTOCOLS: usize = 8;

type CallSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl Relay {
    pub(super) async fn open(self: &Arc<Self>, data: Part) -> Part {
        let channel = text(&data, "channel");
        let path = text(&data, "path");
        if channel.is_empty()
            || path != CALL_SOCKET
            || self.channels.lock().await.contains_key(channel)
        {
            return open_error(404, "request.not_found");
        }
        if self.channels.lock().await.len() >= MAX_CHANNELS {
            return Self::busy("relay.open");
        }
        let request = match upgrade_request(&self.base, path, &data) {
            Ok(request) => request,
            Err(answer) => return answer,
        };
        let websocket = match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => socket,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                return open_error(response.status().as_u16(), "relay.socket_refused")
            }
            Err(_) => return open_error(502, "relay.node_unavailable"),
        };
        let (sender, receiver) = mpsc::channel::<Message>(128);
        let mut channels = self.channels.lock().await;
        if self.closed.load(Ordering::Acquire)
            || channels.len() >= MAX_CHANNELS
            || channels.contains_key(channel)
        {
            return Self::busy("relay.open");
        }
        let channel = channel.to_owned();
        let task = tokio::spawn(self.clone().bridge(channel.clone(), websocket, receiver));
        channels.insert(channel, Channel { sender, task });
        Part::object([("ok", Part::Bool(true))])
    }

    pub(super) async fn data(&self, data: Part) {
        let channel = text(&data, "channel");
        let sender = self
            .channels
            .lock()
            .await
            .get(channel)
            .map(|entry| entry.sender.clone());
        let (Some(sender), Some(message)) = (sender, data.get("data").and_then(to_message)) else {
            return;
        };
        if tokio::time::timeout(Duration::from_millis(250), sender.send(message))
            .await
            .is_err()
        {
            if let Some(entry) = self.channels.lock().await.remove(channel) {
                entry.task.abort();
                let _ = entry.task.await;
                self.close_to_room(channel.to_owned()).await;
            }
        }
    }

    pub(super) async fn close(&self, data: Part) {
        let channel = text(&data, "channel");
        let Some(mut entry) = self.channels.lock().await.remove(channel) else {
            return;
        };
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            entry.sender.send(Message::Close(Some(CloseFrame {
                code: close_code(&data).into(),
                reason: "".into(),
            }))),
        )
        .await;
        drop(entry.sender);
        if tokio::time::timeout(Duration::from_secs(1), &mut entry.task)
            .await
            .is_err()
        {
            entry.task.abort();
        }
    }

    /// Pump one channel both ways until either side ends it, then tell the
    /// room, unless the room already closed it.
    async fn bridge(
        self: Arc<Self>,
        channel: String,
        websocket: CallSocket,
        mut receiver: mpsc::Receiver<Message>,
    ) {
        let (mut sink, mut stream) = websocket.split();
        loop {
            tokio::select! {
                outbound = receiver.recv() => match outbound {
                    Some(message) => if sink.send(message).await.is_err() { break; },
                    None => break,
                },
                incoming = stream.next() => {
                    let payload = match incoming {
                        Some(Ok(Message::Text(text))) => Part::Text(text.to_string()),
                        Some(Ok(Message::Binary(bytes))) => Part::Binary(bytes.to_vec()),
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        _ => continue,
                    };
                    let frame = Part::object([("channel", Part::Text(channel.clone())), ("data", payload)]);
                    if self.outbound.send(("relay.data", frame)).await.is_err() { break; }
                },
            }
        }
        if self.channels.lock().await.remove(&channel).is_some() {
            self.close_to_room(channel).await;
        }
    }
}

fn upgrade_request(base: &Url, path: &str, data: &Part) -> Result<Request, Part> {
    let mut url = base
        .join(path)
        .map_err(|_| open_error(404, "request.not_found"))?;
    let _ = url.set_scheme("ws");
    if let Some(query) = query(data) {
        url.set_query(Some(query));
    }
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| open_error(502, "relay.node_unavailable"))?;
    if let Some(origin) = loopback_origin(base) {
        request.headers_mut().insert(ORIGIN, origin);
    }
    let offered = offered_protocols(data);
    if !offered.is_empty() {
        if let Ok(header) = HeaderValue::from_str(&offered.join(", ")) {
            request
                .headers_mut()
                .insert("sec-websocket-protocol", header);
        }
    }
    Ok(request)
}

fn offered_protocols(data: &Part) -> Vec<&str> {
    match data.get("protocols") {
        Some(Part::Array(parts)) => parts
            .iter()
            .filter_map(Part::text)
            .filter(|p| subprotocol(p))
            .take(MAX_PROTOCOLS)
            .collect(),
        _ => Vec::new(),
    }
}

fn subprotocol(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn to_message(payload: &Part) -> Option<Message> {
    match payload {
        Part::Binary(value) => Some(Message::binary(value.clone())),
        Part::Text(value) => Some(Message::text(value.clone())),
        _ => None,
    }
}

/// The room's close code when it is a valid WebSocket one, else a normal close.
fn close_code(data: &Part) -> u16 {
    match data.get("code") {
        Some(Part::Number(number)) => number
            .as_u64()
            .filter(|n| (1000..5000).contains(n))
            .unwrap_or(1000) as u16,
        _ => 1000,
    }
}
