//! The explanation of a refused stage, derived from the first field that failed.

use serde_json::{Map, Value};

use super::{
    settings::SettingDiagnostic,
    stage::{catalogue_model, provider_serves, valid_model_id},
};
use crate::messages::LocalizedMessage;

pub(super) fn stage_diagnostics(task: &str, field: &str, input: &Value) -> Vec<SettingDiagnostic> {
    let diagnostic = input
        .as_object()
        .and_then(|object| field_diagnostics(task, field, object));
    diagnostic.unwrap_or_else(|| {
        vec![SettingDiagnostic::new(
            task,
            LocalizedMessage::new("settings.stage_invalid"),
        )]
    })
}

/// The specific diagnostics for a stage object, or `None` when only the generic one applies.
fn field_diagnostics(
    task: &str,
    field: &str,
    object: &Map<String, Value>,
) -> Option<Vec<SettingDiagnostic>> {
    let missing = ["place", "model"]
        .into_iter()
        .filter(|required| !object.contains_key(*required))
        .map(|required| {
            SettingDiagnostic::new(
                format!("{task}.{required}"),
                LocalizedMessage::new("settings.field_required"),
            )
        })
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Some(missing);
    }

    if !matches!(
        field,
        "place" | "model" | "build" | "build.engine" | "options"
    ) && !field.starts_with("options.")
    {
        return Some(vec![SettingDiagnostic::new(
            format!("{task}.{field}"),
            LocalizedMessage::new("settings.extra_input"),
        )]);
    }

    let place = object.get("place").and_then(Value::as_str);
    let message = match field {
        "model" => object
            .get("model")
            .and_then(Value::as_str)
            .and_then(|model| model_message(task, model, place)),
        "place" => place.and_then(|place| place_message(task, place)),
        "build" if place.is_some_and(|place| !matches!(place, "device" | "host")) => {
            Some(LocalizedMessage::new("settings.provider_build_invalid"))
        }
        _ => None,
    };
    message.map(|message| vec![SettingDiagnostic::new(task, message)])
}

fn model_message(task: &str, model: &str, place: Option<&str>) -> Option<LocalizedMessage> {
    if !valid_model_id(model) {
        return Some(LocalizedMessage::new("settings.model_id_invalid").with_param("model", model));
    }
    if matches!(place, Some("device" | "host")) && catalogue_model(task, model, None).is_err() {
        return Some(
            LocalizedMessage::new("settings.model_task_invalid")
                .with_param("model", model)
                .with_param("task", task),
        );
    }
    None
}

fn place_message(task: &str, place: &str) -> Option<LocalizedMessage> {
    provider_serves(task, place).is_err().then(|| {
        LocalizedMessage::new("settings.place_task_invalid")
            .with_param("place", place)
            .with_param("task", task)
    })
}
