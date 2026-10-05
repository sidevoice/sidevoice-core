//! Normalisation of what a connector declares about a binding: who it is, what it can do and
//! which engine runs it.
use serde_json::{json, Map, Value};

use super::util::field;

const CAPABILITIES: [&str; 5] = [
    "deliver",
    "inspectInbound",
    "working",
    "endOfTurn",
    "sessionIdentity",
];
const ROUTES: [&str; 4] = [
    "cursor-editor-bridge",
    "cursor-editor-view",
    "cursor-cli-persist",
    "cursor-cli",
];

/// The descriptive part of a registration, already clipped and normalised.
pub(super) struct Declaration {
    pub(super) harness: String,
    pub(super) title: Option<String>,
    pub(super) inbound: Option<Value>,
    pub(super) capabilities: Value,
    pub(super) engine: Option<Value>,
    pub(super) route: Option<String>,
}
impl Declaration {
    pub(super) fn parse(data: &Value) -> Self {
        let route = field(data, "route");
        Self {
            harness: data
                .get("harness")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown")
                .chars()
                .take(40)
                .collect(),
            title: data
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(200).collect()),
            inbound: data.get("inbound").filter(|v| v.is_object()).cloned(),
            capabilities: capabilities(data.get("capabilities"), data.get("experimental")),
            engine: engine(data.get("engine")),
            route: ROUTES.contains(&route).then(|| route.into()),
        }
    }
}

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

fn capabilities(value: Option<&Value>, experimental: Option<&Value>) -> Value {
    let mut result = Map::new();
    for key in CAPABILITIES {
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
        let names: Vec<&str> = CAPABILITIES
            .into_iter()
            .filter(|key| {
                result[*key] == "supported" && marked.iter().any(|v| v.as_str() == Some(key))
            })
            .collect();
        if !names.is_empty() {
            result.insert("experimental".into(), json!(names));
        }
    }
    Value::Object(result)
}
