//! The paired room's four relay events, forwarded to this Core over TCP loopback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::HeaderValue;
use serde_json::json;
use tokio::sync::{mpsc, watch, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use super::packet::Part;
use answer::{http_error, open_error};

mod answer;
mod http;
mod path;
mod socket;
#[cfg(test)]
mod tests;

const MAX_IN_FLIGHT: usize = 32;

pub(super) struct Relay {
    base: Url,
    client: reqwest::Client,
    channels: Mutex<HashMap<String, Channel>>,
    outbound: mpsc::Sender<(&'static str, Part)>,
    requests: Semaphore,
    closed: AtomicBool,
    stopping: watch::Sender<bool>,
}

/// One bridged WebSocket: what the room sends it, and the task pumping it.
struct Channel {
    sender: mpsc::Sender<Message>,
    task: JoinHandle<()>,
}

impl Relay {
    pub(super) fn new(base: Url, outbound: mpsc::Sender<(&'static str, Part)>) -> Self {
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
            requests: Semaphore::new(MAX_IN_FLIGHT),
            closed: AtomicBool::new(false),
            stopping: watch::channel(false).0,
        }
    }

    pub(super) fn busy(event: &str) -> Part {
        if event == "relay.open" {
            open_error(503, "relay.node_unavailable")
        } else {
            http_error(503, "relay.node_unavailable")
        }
    }

    pub(super) fn stopped(&self) -> watch::Receiver<bool> {
        self.stopping.subscribe()
    }

    pub(super) async fn handle(self: &Arc<Self>, event: &str, data: Part) -> Option<Part> {
        match event {
            "relay.http" | "relay.open" => self.answer(event, data).await,
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

    pub(super) async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        self.stopping.send_replace(true);
        let channels = std::mem::take(&mut *self.channels.lock().await);
        for (_, channel) in channels {
            channel.task.abort();
            let _ = channel.task.await;
        }
    }

    /// Answer a request within the in-flight budget, or not at all once stopped.
    async fn answer(self: &Arc<Self>, event: &str, data: Part) -> Option<Part> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let Ok(_permit) = self.requests.try_acquire() else {
            return Some(Self::busy(event));
        };
        let mut stopping = self.stopping.subscribe();
        tokio::select! {
            result = async {
                if event == "relay.http" {
                    http::forward(&self.client, &self.base, data).await
                } else {
                    self.open(data).await
                }
            } => Some(result),
            _ = stopping.changed() => None,
        }
    }

    /// Tell the room a channel closed. If even that cannot be delivered, the
    /// link is stuck and stops.
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
}

fn text<'a>(data: &'a Part, name: &str) -> &'a str {
    data.get(name).and_then(Part::text).unwrap_or("")
}

fn query(data: &Part) -> Option<&str> {
    data.get("query")
        .and_then(Part::text)
        .filter(|query| !query.is_empty())
}

/// Loopback requests present this Core's own origin, never the room's.
fn loopback_origin(base: &Url) -> Option<HeaderValue> {
    HeaderValue::from_str(base.as_str().trim_end_matches('/')).ok()
}
