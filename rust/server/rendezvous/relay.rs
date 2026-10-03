//! The paired room's four relay events, forwarded to this Core over TCP loopback.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, ORIGIN};
use serde_json::json;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::CloseFrame, Message};
use url::Url;

use super::packet::Part;
use super::{relayable, CALL_SOCKET};
use crate::messages::{render, LocalizedMessage};

pub(crate) struct Relay {
    base: Url,
    client: reqwest::Client,
    channels: Mutex<HashMap<String, mpsc::Sender<Message>>>,
    outbound: mpsc::Sender<(&'static str, Part)>,
}

impl Relay {
    pub(crate) fn new(base: Url, outbound: mpsc::Sender<(&'static str, Part)>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .no_proxy()
            .build()
            .expect("static loopback HTTP configuration");
        Self { base, client, channels: Mutex::new(HashMap::new()), outbound }
    }

    pub(crate) async fn handle(self: &Arc<Self>, event: &str, data: Part) -> Option<Part> {
        match event {
            "relay.http" => Some(self.http(data).await),
            "relay.open" => Some(self.open(data).await),
            "relay.data" => { self.data(data).await; None }
            "relay.close" => { self.close(data).await; None }
            _ => None,
        }
    }

    async fn http(&self, data: Part) -> Part {
        let path = data.get("path").and_then(Part::text).unwrap_or("");
        let method = data.get("method").and_then(Part::text).unwrap_or("GET").to_ascii_uppercase();
        if !relayable(&self.base, path, super::super::local_only)
            || !["GET", "POST", "PUT", "DELETE", "PATCH"].contains(&method.as_str())
        {
            return http_error(404, "request.not_found");
        }
        let Ok(mut url) = self.base.join(path) else { return http_error(404, "request.not_found"); };
        if let Some(query) = data.get("query").and_then(Part::text).filter(|query| !query.is_empty()) {
            url.set_query(Some(query));
        }
        let mut headers = HeaderMap::new();
        for (name, header) in [("content-type", CONTENT_TYPE), ("accept", ACCEPT), ("authorization", AUTHORIZATION)] {
            if let Some(value) = data.get("headers").and_then(|v| v.get(name)).and_then(Part::text) {
                if let Ok(value) = HeaderValue::from_str(value) {
                    headers.insert(header, value);
                }
            }
        }
        if data.get("headers").and_then(|v| v.get("origin")).and_then(Part::text).is_some() {
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
                Part::Binary(bytes) => request = request.body(bytes.clone()),
                Part::Text(text) => request = request.body(text.clone()),
                _ => {}
            }
        }
        let Ok(answer) = request.send().await else { return http_error(502, "relay.node_unavailable"); };
        let status = answer.status().as_u16();
        let content_type = answer.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        let Ok(body) = answer.bytes().await else { return http_error(502, "relay.node_unavailable"); };
        Part::object([
            ("status", Part::json(json!(status))),
            ("headers", Part::object([("content-type", Part::Text(content_type))])),
            ("body", Part::Binary(body.to_vec())),
        ])
    }

    async fn open(self: &Arc<Self>, data: Part) -> Part {
        let channel = data.get("channel").and_then(Part::text).unwrap_or("");
        let path = data.get("path").and_then(Part::text).unwrap_or("");
        if channel.is_empty() || path != CALL_SOCKET || self.channels.lock().await.contains_key(channel) {
            return open_error(404, "request.not_found");
        }
        let Ok(mut url) = self.base.join(path) else { return open_error(404, "request.not_found"); };
        let _ = url.set_scheme("ws");
        if let Some(query) = data.get("query").and_then(Part::text).filter(|query| !query.is_empty()) {
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
            Some(Part::Array(parts)) => parts.iter().filter_map(Part::text).filter(|p| subprotocol(p)).take(8).collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        if !offered.is_empty() {
            if let Ok(header) = HeaderValue::from_str(&offered.join(", ")) {
                request.headers_mut().insert("sec-websocket-protocol", header);
            }
        }
        let websocket = match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => socket,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) =>
                return open_error(response.status().as_u16(), "relay.socket_refused"),
            Err(_) => return open_error(502, "relay.node_unavailable"),
        };
        let (sender, mut receiver) = mpsc::channel::<Message>(128);
        {
            let mut channels = self.channels.lock().await;
            if channels.contains_key(channel) { return open_error(404, "request.not_found"); }
            channels.insert(channel.to_owned(), sender);
        }
        let self_ref = self.clone();
        let channel = channel.to_owned();
        tokio::spawn(async move {
            let (mut sink, mut stream) = websocket.split();
            loop {
                tokio::select! {
                    outbound = receiver.recv() => match outbound {
                        Some(message) => if sink.send(message).await.is_err() { break; },
                        None => break,
                    },
                    incoming = stream.next() => match incoming {
                        Some(Ok(Message::Text(text))) => {
                            let frame = Part::object([("channel", Part::Text(channel.clone())), ("data", Part::Text(text.to_string()))]);
                            if self_ref.outbound.send(("relay.data", frame)).await.is_err() { break; }
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            let frame = Part::object([("channel", Part::Text(channel.clone())), ("data", Part::Binary(bytes.to_vec()))]);
                            if self_ref.outbound.send(("relay.data", frame)).await.is_err() { break; }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        _ => {},
                    },
                }
            }
            if self_ref.channels.lock().await.remove(&channel).is_some() {
                let _ = self_ref.outbound.send(("relay.close", Part::object([
                    ("channel", Part::Text(channel)), ("code", Part::json(json!(1000))),
                    ("reason", Part::Text(String::new())),
                ]))).await;
            }
        });
        Part::object([("ok", Part::Bool(true))])
    }

    async fn data(&self, data: Part) {
        let channel = data.get("channel").and_then(Part::text).unwrap_or("");
        let sender = self.channels.lock().await.get(channel).cloned();
        if let (Some(sender), Some(payload)) = (sender, data.get("data")) {
            let message = match payload {
                Part::Binary(value) => Some(Message::binary(value.clone())),
                Part::Text(value) => Some(Message::text(value.clone())),
                _ => None,
            };
            if let Some(message) = message { let _ = sender.send(message).await; }
        }
    }

    async fn close(&self, data: Part) {
        let channel = data.get("channel").and_then(Part::text).unwrap_or("");
        let sender = self.channels.lock().await.remove(channel);
        if let Some(sender) = sender {
            let code = match data.get("code") {
                Some(Part::Number(number)) => number.as_u64().filter(|n| (1000..5000).contains(n)).unwrap_or(1000) as u16,
                _ => 1000,
            };
            let _ = sender.send(Message::Close(Some(CloseFrame { code: code.into(), reason: "".into() }))).await;
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.channels.lock().await.clear();
    }
}

fn subprotocol(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|b| {
        b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
    })
}

fn http_error(status: u16, key: &str) -> Part {
    let detail = render(&LocalizedMessage::new(key), "en");
    Part::object([
        ("status", Part::json(json!(status))),
        ("headers", Part::object([("content-type", Part::Text("application/json".into()))])),
        ("body", Part::Binary(json!({"detail": detail}).to_string().into_bytes())),
    ])
}

fn open_error(status: u16, key: &str) -> Part {
    Part::object([
        ("ok", Part::Bool(false)),
        ("status", Part::json(json!(status))),
        ("detail", Part::Text(render(&LocalizedMessage::new(key), "en"))),
    ])
}
