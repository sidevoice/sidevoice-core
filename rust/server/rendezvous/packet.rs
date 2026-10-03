//! Socket.IO JSON placeholders with binary attachments. `sioc` owns packet
//! framing, namespace handling, ACK IDs and reconnect signals; this module
//! only maps the current relay's nested application payloads.

use serde_json::{Map, Number, Value};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Part {
    Null,
    Bool(bool),
    Number(Number),
    Text(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
    Binary(Vec<u8>),
}

impl Part {
    pub(crate) fn get(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, part)| part),
            _ => None,
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn object(fields: impl IntoIterator<Item = (&'static str, Self)>) -> Self {
        Self::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    pub(crate) fn json(value: Value) -> Self {
        decode(value, &[]).expect("JSON has no binary placeholders")
    }
}

/// Socketioxide's common parser can deserialize nested binary into `rmpv`.
/// Keep it binary when forwarding instead of turning it into JSON numbers.
pub(crate) fn from_rmpv(value: rmpv::Value) -> Option<Part> {
    match value {
        rmpv::Value::Nil => Some(Part::Null),
        rmpv::Value::Boolean(value) => Some(Part::Bool(value)),
        rmpv::Value::Integer(value) => Some(Part::Number(if let Some(number) = value.as_i64() {
            number.into()
        } else {
            value.as_u64()?.into()
        })),
        rmpv::Value::F32(value) => Number::from_f64(f64::from(value)).map(Part::Number),
        rmpv::Value::F64(value) => Number::from_f64(value).map(Part::Number),
        rmpv::Value::String(value) => value.as_str().map(|text| Part::Text(text.to_owned())),
        rmpv::Value::Binary(value) => Some(Part::Binary(value)),
        rmpv::Value::Array(values) => values
            .into_iter()
            .map(from_rmpv)
            .collect::<Option<Vec<_>>>()
            .map(Part::Array),
        rmpv::Value::Map(values) => values
            .into_iter()
            .map(|(key, value)| Some((key.as_str()?.to_owned(), from_rmpv(value)?)))
            .collect::<Option<Vec<_>>>()
            .map(Part::Object),
        _ => None,
    }
}

pub(crate) fn to_rmpv(part: Part) -> rmpv::Value {
    match part {
        Part::Null => rmpv::Value::Nil,
        Part::Bool(value) => rmpv::Value::Boolean(value),
        Part::Number(value) => {
            if let Some(number) = value.as_i64() {
                rmpv::Value::from(number)
            } else if let Some(number) = value.as_u64() {
                rmpv::Value::from(number)
            } else {
                rmpv::Value::from(value.as_f64().unwrap_or_default())
            }
        }
        Part::Text(value) => rmpv::Value::from(value),
        Part::Binary(value) => rmpv::Value::Binary(value),
        Part::Array(values) => rmpv::Value::Array(values.into_iter().map(to_rmpv).collect()),
        Part::Object(values) => rmpv::Value::Map(
            values
                .into_iter()
                .map(|(key, value)| (rmpv::Value::from(key), to_rmpv(value)))
                .collect(),
        ),
    }
}

/// Reassemble every nested attachment. A placeholder outside the declared
/// attachment list is malformed and must never reach an endpoint.
pub(crate) fn decode(value: Value, attachments: &[Vec<u8>]) -> Option<Part> {
    match value {
        Value::Null => Some(Part::Null),
        Value::Bool(value) => Some(Part::Bool(value)),
        Value::Number(value) => Some(Part::Number(value)),
        Value::String(value) => Some(Part::Text(value)),
        Value::Array(values) => values
            .into_iter()
            .map(|value| decode(value, attachments))
            .collect::<Option<Vec<_>>>()
            .map(Part::Array),
        Value::Object(values) => {
            if values.get("_placeholder") == Some(&Value::Bool(true)) {
                let index = values.get("num")?.as_u64()? as usize;
                if values.len() != 2 {
                    return None;
                }
                return attachments.get(index).cloned().map(Part::Binary);
            }
            values
                .into_iter()
                .map(|(key, value)| decode(value, attachments).map(|part| (key, part)))
                .collect::<Option<Vec<_>>>()
                .map(Part::Object)
        }
    }
}

/// Produce the exact JSON placeholder tree and attachment sequence expected
/// by the Socket.IO library's dynamic packet API.
pub(crate) fn encode(part: Part, attachments: &mut Vec<Vec<u8>>) -> Value {
    match part {
        Part::Null => Value::Null,
        Part::Bool(value) => Value::Bool(value),
        Part::Number(value) => Value::Number(value),
        Part::Text(value) => Value::String(value),
        Part::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| encode(value, attachments))
                .collect(),
        ),
        Part::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, encode(value, attachments)))
                .collect::<Map<String, Value>>(),
        ),
        Part::Binary(value) => {
            let index = attachments.len();
            attachments.push(value);
            serde_json::json!({"_placeholder": true, "num": index})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_binary_payload_keeps_envelope_and_attachment_order() {
        let raw = serde_json::json!({"channel":"c-1","data":{"voice":{"_placeholder":true,"num":1},"other":[{"_placeholder":true,"num":0}]}});
        let attachments = vec![b"first".to_vec(), b"second".to_vec()];
        let decoded = decode(raw.clone(), &attachments).unwrap();
        let mut encoded = Vec::new();
        let rebuilt = encode(decoded.clone(), &mut encoded);
        assert_eq!(decode(rebuilt, &encoded), Some(decoded));
        assert_eq!(encoded.len(), attachments.len());
    }

    #[test]
    fn missing_attachment_is_rejected() {
        let raw = serde_json::json!({"body":{"_placeholder":true,"num":2}});
        assert!(decode(raw, &[b"only".to_vec()]).is_none());
    }
}
