//! The paired room's four relay events, forwarded to this Core over TCP loopback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, ORIGIN};
use serde_json::json;
use tokio::sync::{mpsc, watch, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::CloseFrame, Message};
use url::Url;

use super::packet::Part;
use super::{relayable, CALL_SOCKET};
use crate::messages::{render, LocalizedMessage};

pub(crate) struct Relay {
    base: Url,
    client: reqwest::Client,
    channels: Mutex<HashMap<String, Channel>>,
    outbound: mpsc::Sender<(&'static str, Part)>,
    requests: Semaphore,
    closed: AtomicBool,
    stopping: watch::Sender<bool>,
}

struct Channel {
    sender: mpsc::Sender<Message>,
    task: JoinHandle<()>,
}

impl Relay {
    pub(crate) fn new(base: Url, outbound: mpsc::Sender<(&'static str, Part)>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .no_proxy()
            .build()
            .expect("static loopback HTTP configuration");
        Self {
            base,
            client,
            channels: Mutex::new(HashMap::new()),
            outbound,
            requests: Semaphore::new(32),
            closed: AtomicBool::new(false),
            stopping: watch::channel(false).0,
        }
    }

    pub(crate) fn busy(event: &str) -> Part {
        if event == "relay.open" {
            open_error(503, "relay.node_unavailable")
        } else {
            http_error(503, "relay.node_unavailable")
        }
    }

    pub(super) fn stopped(&self) -> watch::Receiver<bool> {
        self.stopping.subscribe()
    }

    async fn close_to_room(&self, channel: String) {
        let frame = Part::object([
            ("channel", Part::Text(channel)),
            ("code", Part::json(json!(1000))),
            ("reason", Part::Text(String::new())),
        ]);
        if !matches!(
            tokio::time::timeout(
                Duration::from_millis(250),
                self.outbound.send(("relay.close", frame))
            )
            .await,
            Ok(Ok(()))
        ) {
            self.closed.store(true, Ordering::Release);
            self.stopping.send_replace(true);
        }
    }

    pub(crate) async fn handle(self: &Arc<Self>, event: &str, data: Part) -> Option<Part> {
        match event {
            "relay.http" | "relay.open" => {
                if self.closed.load(Ordering::Acquire) {
                    return None;
                }
                let Ok(_permit) = self.requests.try_acquire() else {
                    return Some(Self::busy(event));
                };
                let mut stopping = self.stopping.subscribe();
                tokio::select! {
                    result = async {
                        if event == "relay.http" { self.http(data).await } else { self.open(data).await }
                    } => Some(result),
                    _ = stopping.changed() => None,
                }
            }
            "relay.data" => {
                self.data(data).await;
                None
            }
            "relay.close" => {
                self.close(data).await;
                None
            }
            _ => None,
        }
    }

    async fn http(&self, data: Part) -> Part {
        let path = data.get("path").and_then(Part::text).unwrap_or("");
        let method = data
            .get("method")
            .and_then(Part::text)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        if !relayable(&self.base, path, super::super::local_only)
            || !["GET", "POST", "PUT", "DELETE", "PATCH"].contains(&method.as_str())
        {
            return http_error(404, "request.not_found");
        }
        let Ok(mut url) = self.base.join(path) else {
            return http_error(404, "request.not_found");
        };
        if let Some(query) = data
            .get("query")
            .and_then(Part::text)
            .filter(|query| !query.is_empty())
        {
            url.set_query(Some(query));
        }
        let mut headers = HeaderMap::new();
        for (name, header) in [
            ("content-type", CONTENT_TYPE),
            ("accept", ACCEPT),
            ("authorization", AUTHORIZATION),
        ] {
            if let Some(value) = data
                .get("headers")
                .and_then(|v| v.get(name))
                .and_then(Part::text)
            {
                if let Ok(value) = HeaderValue::from_str(value) {
                    headers.insert(header, value);
                }
            }
        }
        if data
            .get("headers")
            .and_then(|v| v.get("origin"))
            .and_then(Part::text)
            .is_some()
        {
            if let Ok(origin) = HeaderValue::from_str(self.base.as_str().trim_end_matches('/')) {
                headers.insert(ORIGIN, origin);
            }
        }
        let Some(method) = reqwest::Method::from_bytes(method.as_bytes()).ok() else {
            return http_error(404, "request.not_found");
        };
        let mut request = self.client.request(method, url).headers(headers);
        if let Some(body) = data.get("body") {
            match body {
                Part::Binary(bytes) if bytes.len() <= 8 * 1024 * 1024 => {
                    request = request.body(bytes.clone())
                }
                Part::Text(text) if text.len() <= 8 * 1024 * 1024 => {
                    request = request.body(text.clone())
                }
                Part::Binary(_) | Part::Text(_) => {
                    return http_error(502, "relay.node_unavailable")
                }
                _ => {}
            }
        }
        let Ok(answer) = request.send().await else {
            return http_error(502, "relay.node_unavailable");
        };
        let status = answer.status().as_u16();
        let content_type = answer
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let mut answer = answer;
        let mut body = Vec::new();
        loop {
            match answer.chunk().await {
                Ok(Some(chunk)) if body.len().saturating_add(chunk.len()) <= 8 * 1024 * 1024 => {
                    body.extend_from_slice(&chunk)
                }
                Ok(None) => break,
                _ => return http_error(502, "relay.node_unavailable"),
            }
        }
        Part::object([
            ("status", Part::json(json!(status))),
            (
                "headers",
                Part::object([("content-type", Part::Text(content_type))]),
            ),
            ("body", Part::Binary(body)),
        ])
    }

    async fn open(self: &Arc<Self>, data: Part) -> Part {
        let channel = data.get("channel").and_then(Part::text).unwrap_or("");
        let path = data.get("path").and_then(Part::text).unwrap_or("");
        if channel.is_empty()
            || path != CALL_SOCKET
            || self.channels.lock().await.contains_key(channel)
        {
            return open_error(404, "request.not_found");
        }
        if self.channels.lock().await.len() >= 32 {
            return Self::busy("relay.open");
        }
        let Ok(mut url) = self.base.join(path) else {
            return open_error(404, "request.not_found");
        };
        let _ = url.set_scheme("ws");
        if let Some(query) = data
            .get("query")
            .and_then(Part::text)
            .filter(|query| !query.is_empty())
        {
            url.set_query(Some(query));
        }
        let mut request = match url.as_str().into_client_request() {
            Ok(request) => request,
            Err(_) => return open_error(502, "relay.node_unavailable"),
        };
        if let Ok(origin) = HeaderValue::from_str(self.base.as_str().trim_end_matches('/')) {
            request.headers_mut().insert(ORIGIN, origin);
        }
        let offered = match data.get("protocols") {
            Some(Part::Array(parts)) => parts
                .iter()
                .filter_map(Part::text)
                .filter(|p| subprotocol(p))
                .take(8)
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        if !offered.is_empty() {
            if let Ok(header) = HeaderValue::from_str(&offered.join(", ")) {
                request
                    .headers_mut()
                    .insert("sec-websocket-protocol", header);
            }
        }
        let websocket = match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => socket,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                return open_error(response.status().as_u16(), "relay.socket_refused")
            }
            Err(_) => return open_error(502, "relay.node_unavailable"),
        };
        let (sender, mut receiver) = mpsc::channel::<Message>(128);
        let mut channels = self.channels.lock().await;
        if self.closed.load(Ordering::Acquire)
            || channels.len() >= 32
            || channels.contains_key(channel)
        {
            return Self::busy("relay.open");
        }
        let self_ref = self.clone();
        let channel = channel.to_owned();
        let task_channel = channel.clone();
        let task = tokio::spawn(async move {
            let (mut sink, mut stream) = websocket.split();
            loop {
                tokio::select! {
                    outbound = receiver.recv() => match outbound {
                        Some(message) => if sink.send(message).await.is_err() { break; },
                        None => break,
                    },
                    incoming = stream.next() => match incoming {
                        Some(Ok(Message::Text(text))) => {
                            let frame = Part::object([("channel", Part::Text(task_channel.clone())), ("data", Part::Text(text.to_string()))]);
                            if self_ref.outbound.send(("relay.data", frame)).await.is_err() { break; }
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            let frame = Part::object([("channel", Part::Text(task_channel.clone())), ("data", Part::Binary(bytes.to_vec()))]);
                            if self_ref.outbound.send(("relay.data", frame)).await.is_err() { break; }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        _ => {},
                    },
                }
            }
            if self_ref
                .channels
                .lock()
                .await
                .remove(&task_channel)
                .is_some()
            {
                self_ref.close_to_room(task_channel).await;
            }
        });
        channels.insert(channel, Channel { sender, task });
        Part::object([("ok", Part::Bool(true))])
    }

    async fn data(&self, data: Part) {
        let channel = data.get("channel").and_then(Part::text).unwrap_or("");
        let sender = self
            .channels
            .lock()
            .await
            .get(channel)
            .map(|entry| entry.sender.clone());
        if let (Some(sender), Some(payload)) = (sender, data.get("data")) {
            let message = match payload {
                Part::Binary(value) => Some(Message::binary(value.clone())),
                Part::Text(value) => Some(Message::text(value.clone())),
                _ => None,
            };
            if let Some(message) = message {
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
        }
    }

    async fn close(&self, data: Part) {
        let channel = data.get("channel").and_then(Part::text).unwrap_or("");
        let sender = self.channels.lock().await.remove(channel);
        if let Some(mut entry) = sender {
            let code = match data.get("code") {
                Some(Part::Number(number)) => number
                    .as_u64()
                    .filter(|n| (1000..5000).contains(n))
                    .unwrap_or(1000) as u16,
                _ => 1000,
            };
            let _ = tokio::time::timeout(
                Duration::from_millis(250),
                entry.sender.send(Message::Close(Some(CloseFrame {
                    code: code.into(),
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
    }

    pub(crate) async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        self.stopping.send_replace(true);
        let channels = std::mem::take(&mut *self.channels.lock().await);
        for (_, channel) in channels {
            channel.task.abort();
            let _ = channel.task.await;
        }
    }
}

fn subprotocol(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn http_error(status: u16, key: &str) -> Part {
    let detail = render(&LocalizedMessage::new(key), "en");
    Part::object([
        ("status", Part::json(json!(status))),
        (
            "headers",
            Part::object([("content-type", Part::Text("application/json".into()))]),
        ),
        (
            "body",
            Part::Binary(json!({"detail": detail}).to_string().into_bytes()),
        ),
    ])
}

fn open_error(status: u16, key: &str) -> Part {
    Part::object([
        ("ok", Part::Bool(false)),
        ("status", Part::json(json!(status))),
        (
            "detail",
            Part::Text(render(&LocalizedMessage::new(key), "en")),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn full_channel_reports_one_close_or_stops_the_link() {
        let (outbound, mut room) = mpsc::channel(1);
        let relay = Arc::new(Relay::new(
            Url::parse("http://127.0.0.1:8768/").unwrap(),
            outbound,
        ));
        let (sender, _receiver) = mpsc::channel(1);
        sender.send(Message::text("held")).await.unwrap();
        let task = tokio::spawn(std::future::pending());
        relay
            .channels
            .lock()
            .await
            .insert("blocked".into(), Channel { sender, task });
        tokio::time::timeout(
            Duration::from_secs(1),
            relay.handle(
                "relay.data",
                Part::object([
                    ("channel", Part::Text("blocked".into())),
                    ("data", Part::Binary(vec![0, 1])),
                ]),
            ),
        )
        .await
        .unwrap();
        let (event, frame) = room.recv().await.unwrap();
        assert_eq!(event, "relay.close");
        assert_eq!(frame.get("channel").and_then(Part::text), Some("blocked"));
        assert!(relay.channels.lock().await.is_empty());
        let _ = relay
            .handle(
                "relay.data",
                Part::object([
                    ("channel", Part::Text("blocked".into())),
                    ("data", Part::Text("late".into())),
                ]),
            )
            .await;
        assert!(
            room.try_recv().is_err(),
            "the room must see exactly one close"
        );
        tokio::time::timeout(Duration::from_secs(1), relay.shutdown())
            .await
            .unwrap();

        let (outbound, _room) = mpsc::channel(1);
        outbound.send(("relay.data", Part::Null)).await.unwrap();
        let relay = Arc::new(Relay::new(
            Url::parse("http://127.0.0.1:8768/").unwrap(),
            outbound,
        ));
        let (sender, _receiver) = mpsc::channel(1);
        sender.send(Message::text("held")).await.unwrap();
        let task = tokio::spawn(std::future::pending());
        relay
            .channels
            .lock()
            .await
            .insert("blocked".into(), Channel { sender, task });
        tokio::time::timeout(
            Duration::from_secs(1),
            relay.handle(
                "relay.data",
                Part::object([
                    ("channel", Part::Text("blocked".into())),
                    ("data", Part::Binary(vec![0, 1])),
                ]),
            ),
        )
        .await
        .unwrap();
        let stopped = relay.stopped();
        assert!(
            *stopped.borrow(),
            "an undeliverable close must stop the link"
        );
        relay.shutdown().await;
    }

    #[tokio::test]
    async fn in_flight_budget_refuses_excess_and_shutdown_cancels_request() {
        let (outbound, _) = mpsc::channel(1);
        let relay = Arc::new(Relay::new(
            Url::parse("http://127.0.0.1:8768/").unwrap(),
            outbound,
        ));
        let permits = relay.requests.acquire_many(32).await.unwrap();
        let answer = relay
            .handle(
                "relay.http",
                Part::object([("path", Part::Text("/api/presentation".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(answer.get("status"), Some(&Part::json(json!(503))));
        drop(permits);
        relay.shutdown().await;
        assert!(relay.handle("relay.http", Part::Null).await.is_none());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (outbound, _) = mpsc::channel(1);
        let relay = Arc::new(Relay::new(base, outbound));
        let request_relay = relay.clone();
        let request = tokio::spawn(async move {
            request_relay
                .handle(
                    "relay.http",
                    Part::object([("path", Part::Text("/api/presentation".into()))]),
                )
                .await
        });
        let (_socket, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        relay.shutdown().await;
        assert!(tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .unwrap()
            .unwrap()
            .is_none());
    }
}
