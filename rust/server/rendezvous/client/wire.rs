//! `Part` payloads to and from `sioc` events and acknowledgements, as text
//! packets or, when they carry binary, as packets with attachments.

use bytes::Bytes;
use serde_json::{json, Value};
use sioc::client::{Acknowledge, Emit, SocketSender};
use sioc::marker::{AckMarker, HasAck, HasBinary, NoAck, NoBinary};
use sioc::packet::{Directive, DynEvent};
use sioc::prelude::AckType;

use super::super::packet::{decode, encode, Part};

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

pub(super) async fn emit(sender: &SocketSender, event: &str, part: Part) -> bool {
    let mut attachments = Vec::new();
    let payload = json!([event, encode(part, &mut attachments)]).to_string();
    if attachments.is_empty() {
        sender.emit(TextEvent(payload)).await.is_ok()
    } else {
        sender
            .emit(BinaryEvent(payload, into_bytes(attachments)))
            .await
            .is_ok()
    }
}

pub(super) async fn acknowledge(sender: &SocketSender, id: u64, answer: Part) {
    let mut attachments = Vec::new();
    let payload = json!([encode(answer, &mut attachments)]).to_string();
    if attachments.is_empty() {
        if let Ok(id) = <HasAck<TextAck> as AckMarker>::parse(Some(id)) {
            let _ = sender.acknowledge(id, TextAnswer(payload)).await;
        }
    } else if let Ok(id) = <HasAck<BinaryAck> as AckMarker>::parse(Some(id)) {
        let _ = sender
            .acknowledge(id, BinaryAnswer(payload, into_bytes(attachments)))
            .await;
    }
}

/// An event's name, its payload with attachments reassembled, and its ACK id.
pub(super) fn parse(event: DynEvent) -> Option<(String, Part, Option<u64>)> {
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

fn into_bytes(attachments: Vec<Vec<u8>>) -> Vec<Bytes> {
    attachments.into_iter().map(Bytes::from).collect()
}
