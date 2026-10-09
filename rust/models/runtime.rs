//! The transcription runtime a client reports, and what the room may show about a call's transcription.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::device_models::{model_for, offered, runs_on};
use crate::{
    messages::LocalizedMessage,
    types::{DeviceModels, SpeechStage},
};

/// The transcription runtime a client reports (in its hello and in `voice-stt-ready`), or `None` when it
/// reports none: a reported model of the stt task, on the backend of one of its available builds, and the
/// accelerator it runs on. What the stats show, not what the node obeys. A load that fell back keeps its reason.
/// `Err` is a report this node cannot read.
pub fn browser_runtime(
    data: Option<&Value>,
    report: Option<&Arc<DeviceModels>>,
) -> Result<Option<Map<String, Value>>, LocalizedMessage> {
    let Some(data) = data.and_then(Value::as_object) else {
        return Ok(None);
    };
    let model = data
        .get("model")
        .and_then(Value::as_str)
        .and_then(|model| model_for(offered(report), "stt", model))
        .ok_or_else(unsupported_runtime)?;
    let engine = data
        .get("engine")
        .and_then(Value::as_str)
        .ok_or_else(unsupported_runtime)?;
    if !runs_on(model, engine) {
        return Err(unsupported_runtime());
    }
    let accelerator = data
        .get("accelerator")
        .and_then(Value::as_str)
        .filter(|value| (1..=40).contains(&value.chars().count()))
        .ok_or_else(unsupported_runtime)?;
    let mut runtime = Map::new();
    runtime.insert("model".into(), Value::from(model.id.as_str()));
    runtime.insert("engine".into(), Value::from(engine));
    runtime.insert("accelerator".into(), Value::from(accelerator));
    runtime.insert(
        "cached".into(),
        Value::Bool(data.get("cached") == Some(&Value::Bool(true))),
    );
    let text = |value: Option<&Value>, max: usize| -> String {
        let text = match value {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
        };
        text.chars().take(max).collect()
    };
    let fallback_error = text(data.get("fallback_error"), 300);
    if !fallback_error.is_empty() {
        runtime.insert(
            "fallback_from".into(),
            Value::from(text(data.get("fallback_from"), 20)),
        );
        runtime.insert("fallback_error".into(), Value::from(fallback_error));
    }
    Ok(Some(runtime))
}

fn unsupported_runtime() -> LocalizedMessage {
    LocalizedMessage::new("voice.transcription_runtime_unsupported")
}

/// What the room may show about a call's transcription: where and with which model, in which language
/// (none when detected), and the runtime the client reported. Never the stage's context, which is the
/// person's own words to the recogniser.
pub fn call_transcription(stage: &SpeechStage, runtime: Option<&Map<String, Value>>) -> Value {
    let mut view = Map::new();
    view.insert("place".into(), Value::from(stage.place.as_str()));
    view.insert("model".into(), Value::from(stage.model.as_str()));
    view.insert(
        "language".into(),
        stage
            .options
            .get("language")
            .filter(|language| language.as_str() != Some("auto"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    view.insert("available".into(), Value::Bool(true));
    if let Some(runtime) = runtime {
        view.extend(runtime.clone());
    }
    Value::Object(view)
}
