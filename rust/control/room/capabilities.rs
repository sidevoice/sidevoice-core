//! Normalisation of what a connector declares about a binding: its capabilities and its engine.
use serde_json::{json, Map, Value};

pub(super) fn engine(value: Option<&Value>) -> Option<Value> {
    let obj = value?.as_object()?;
    let mut out = Map::new();
    for key in ["model", "effort", "thinking"] {
        if let Some(v) = obj
            .get(key)
            .filter(|v| !v.is_null() && v.as_str() != Some(""))
        {
            out.insert(
                key.into(),
                json!(v
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
                    .chars()
                    .take(60)
                    .collect::<String>()),
            );
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}
pub(super) fn capabilities(value: Option<&Value>, experimental: Option<&Value>) -> Value {
    let mut result = Map::new();
    for key in [
        "deliver",
        "inspectInbound",
        "working",
        "endOfTurn",
        "sessionIdentity",
    ] {
        let status = value
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .filter(|s| ["supported", "unsupported"].contains(s))
            .unwrap_or("unknown");
        result.insert(key.into(), json!(status));
    }
    let marked = experimental
        .or_else(|| value.and_then(|v| v.get("experimental")))
        .and_then(Value::as_array);
    if let Some(marked) = marked {
        let names: Vec<&str> = [
            "deliver",
            "inspectInbound",
            "working",
            "endOfTurn",
            "sessionIdentity",
        ]
        .into_iter()
        .filter(|key| result[*key] == "supported" && marked.iter().any(|v| v.as_str() == Some(key)))
        .collect();
        if !names.is_empty() {
            result.insert("experimental".into(), json!(names));
        }
    }
    Value::Object(result)
}
