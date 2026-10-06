//! The OTLP/HTTP JSON encoding of one stage observation.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// One histogram data point for `stage`, carrying the already-allowed `attributes` object.
pub(super) fn histogram(stage: &str, milliseconds: f64, attributes: &Value) -> Value {
    let attrs = attributes
        .as_object()
        .expect("attributes object")
        .iter()
        .map(|(key, value)| json!({"key":key,"value":any_value(value)}))
        .collect::<Vec<_>>();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string();
    json!({"resourceMetrics":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"sidevoice-core"}}]},
        "scopeMetrics":[{"scope":{"name":"sidevoice.room"},"metrics":[{"name":format!("sidevoice.turn.{stage}"),"unit":"ms",
            "histogram":{"aggregationTemporality":2,"dataPoints":[{"attributes":attrs,"timeUnixNano":now,
                "count":"1","sum":milliseconds,"bucketCounts":["1"]}]}}]}]}]})
}

fn any_value(value: &Value) -> Value {
    match value {
        Value::String(text) => json!({"stringValue":text}),
        Value::Bool(value) => json!({"boolValue":value}),
        Value::Number(value) => json!({"doubleValue":value.as_f64().unwrap_or_default()}),
        _ => Value::Null,
    }
}
