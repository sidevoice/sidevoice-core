//! Lenient readers over untyped catalogue JSON: a missing or mistyped field reads as empty.

use serde_json::Value;

pub(super) fn values(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

pub(super) fn strings(value: Option<&Value>) -> impl Iterator<Item = &str> {
    value.into_iter().flat_map(values).filter_map(Value::as_str)
}

pub(super) fn field_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
