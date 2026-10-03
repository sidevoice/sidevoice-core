//! Outbound `/nodes` Socket.IO client. `sioc` owns framing, attachment
//! reassembly, namespace auth, ACK correlation and Engine.IO heartbeats.

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{json, Value};
use sioc::client::{Acknowledge, ClientBuilder, Emit, SocketSender};
use sioc::marker::{AckMarker, HasAck, HasBinary, NoAck, NoBinary};
use sioc::packet::{Directive, DynEvent, Signal};
use sioc::prelude::{AckType, TransportStrategy};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, MissedTickBehavior};

use super::packet::{decode, encode, Part};
use super::relay::Relay;
use super::{Pairing, Rendezvous, OUTBOUND_NAMESPACE, OUTBOUND_PATH};
use crate::messages::{render, LocalizedMessage};

struct TextEvent(String);
impl Emit<NoAck, NoBinary> for TextEvent {
    type Output = ();
    fn prepare(self) -> Result<(Directive, ()), sioc::error::PayloadError> {
        Ok((
            Directive::Event {
                payload: self.0.into(),
                tx: None,
                attachments: None,
            },
            (),
        ))
    }
}

struct BinaryEvent(String, Vec<Bytes>);
impl Emit<NoAck, HasBinary> for BinaryEvent {
    type Output = ();
    fn prepare(self) -> Result<(Directive, ()), sioc::error::PayloadError> {
        Ok((
            Directive::Event {
                payload: self.0.into(),
                tx: None,
                attachments: Some(self.1),
            },
            (),
        ))
    }
}

struct TextAck;
impl AckType for TextAck {
    type Binary = NoBinary;
}
struct BinaryAck;
impl AckType for BinaryAck {
    type Binary = HasBinary;
}

struct TextAnswer(String);
impl Acknowledge<TextAck, NoBinary> for TextAnswer {
    fn into_directive(self, id: u64) -> Result<Directive, sioc::error::PayloadError> {
        Ok(Directive::Ack {
            payload: self.0.into(),
            id,
            attachments: None,
        })
    }
}

struct BinaryAnswer(String, Vec<Bytes>);
impl Acknowledge<BinaryAck, HasBinary> for BinaryAnswer {
    fn into_directive(self, id: u64) -> Result<Directive, sioc::error::PayloadError> {
        Ok(Directive::Ack {
            payload: self.0.into(),
            id,
            attachments: Some(self.1),
        })
    }
}

pub(super) async fn emit(sender: &SocketSender, event: &str, part: Part) {
    let mut attachments = Vec::new();
    let payload = json!([event, encode(part, &mut attachments)]).to_string();
    if attachments.is_empty() {
        let _ = sender.emit(TextEvent(payload)).await;
    } else {
        let _ = sender
            .emit(BinaryEvent(
                payload,
                attachments.into_iter().map(Bytes::from).collect(),
            ))
            .await;
    }
}

async fn acknowledge(sender: &SocketSender, id: u64, answer: Part) {
    let mut attachments = Vec::new();
    let payload = json!([encode(answer, &mut attachments)]).to_string();
    if attachments.is_empty() {
        if let Ok(id) = <HasAck<TextAck> as AckMarker>::parse(Some(id)) {
            let _ = sender.acknowledge(id, TextAnswer(payload)).await;
        }
    } else if let Ok(id) = <HasAck<BinaryAck> as AckMarker>::parse(Some(id)) {
        let _ = sender
            .acknowledge(
                id,
                BinaryAnswer(payload, attachments.into_iter().map(Bytes::from).collect()),
            )
            .await;
    }
}

fn parse(event: DynEvent) -> Option<(String, Part, Option<u64>)> {
    let fields: Value = serde_json::from_str(&event.payload).ok()?;
    let values = fields.as_array()?;
    let name = values.first()?.as_str()?.to_owned();
    let data = values.get(1).cloned().unwrap_or(Value::Null);
    let attachments = event
        .attachments
        .unwrap_or_default()
        .into_iter()
        .map(|bytes| bytes.to_vec())
        .collect::<Vec<_>>();
    Some((name, decode(data, &attachments)?, event.id))
}

/// One dial attempt. The parent watcher retries transport failures and stops
/// on a room-supplied refusal until the connector changes the pairing file.
pub(super) async fn run(rv: Arc<Rendezvous>, pairing: Pairing) -> Result<(), ()> {
    let url = url::Url::parse(&pairing.origin).map_err(|_| ())?;
    let client = ClientBuilder::new(url)
        .path(OUTBOUND_PATH.trim_start_matches('/'))
        .transport(TransportStrategy::WebSocket)
        .open()
        .map_err(|_| ())?;
    let auth = rv.identity(&pairing);
    let (sender, mut receiver) = client
        .connect_with(OUTBOUND_NAMESPACE, auth.to_string())
        .await
        .map_err(|_| ())?;
    let (outbound, mut output) = mpsc::channel::<(&'static str, Part)>(128);
    let relay = Arc::new(Relay::new(rv.base.clone(), outbound));
    let mut welcomed = false;
    let mut requests = JoinSet::new();
    let mut pairing_check = tokio::time::interval(Duration::from_secs(2));
    pairing_check.set_missed_tick_behavior(MissedTickBehavior::Skip);
    pairing_check.tick().await;
    let mut stopping = rv.stopping.subscribe();
    loop {
        tokio::select! {
            _ = pairing_check.tick() => {
                if rv.current_pairing().as_ref() != Some(&pairing) { break; }
            },
            finished = requests.join_next(), if !requests.is_empty() => { let _ = finished; },
            signal = receiver.recv() => match signal {
                Some(Signal::Connect(_)) => {},
                Some(Signal::ConnectError(error)) => {
                    rv.refused(error.message.to_string()).await;
                    break;
                }
                Some(Signal::Disconnect) | None => break,
                Some(Signal::Event(event)) => {
                    let Some((name, data, id)) = parse(event) else { continue; };
                    match name.as_str() {
                        "node.welcome" => {
                            welcomed = true;
                            let public = data.get("public_url").and_then(Part::text).and_then(super::public_origin);
                            rv.connected("outbound", public).await;
                        }
                        "node.revoked" => {
                            let reason = data
                                .get("reason")
                                .and_then(Part::text)
                                .map(str::to_owned)
                                .unwrap_or_else(|| render(&LocalizedMessage::new("relay.revoked"), "en"));
                            rv.refused(reason).await;
                            break;
                        }
                        "relay.http" | "relay.open" => {
                            if requests.len() >= 32 {
                                if let Some(id) = id {
                                    let answer = Relay::busy(&name);
                                    let _ = tokio::time::timeout(Duration::from_secs(1), acknowledge(&sender, id, answer)).await;
                                }
                            } else {
                                let relay = relay.clone();
                                let sender = sender.clone();
                                requests.spawn(async move {
                                    if let Some(answer) = relay.handle(&name, data).await {
                                        if let Some(id) = id { acknowledge(&sender, id, answer).await; }
                                    }
                                });
                            }
                        }
                        "relay.data" | "relay.close" => { let _ = relay.handle(&name, data).await; }
                        _ => {},
                    }
                }
            },
            outbound = output.recv() => match outbound {
                Some((event, part)) => emit(&sender, event, part).await,
                None => break,
            },
            _ = rv.changed.notified() => break,
            _ = stopping.changed() => break,
        }
    }
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    relay.shutdown().await;
    sender.disconnect().await;
    if welcomed {
        rv.disconnected("outbound").await;
    }
    drop(client);
    Ok(())
}
