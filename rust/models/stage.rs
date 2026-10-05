//! Parsing one speech stage (`stt` or `tts`) from wire JSON with the catalogue's rules.

use serde_json::{Map, Value};

use super::{
    catalog::{find_model, find_provider, model_schema, provider_schema, task_for_model},
    json::{field_str, strings, values},
    options::{validate_options, OptionFailure},
};
use crate::types::{ModelBuild, SpeechStage};

/// Why a stage was refused: the first field that failed, or the first option that failed.
#[derive(Clone, Debug)]
pub(super) enum StageFailure {
    Field(String),
    Option(OptionFailure),
}

impl From<String> for StageFailure {
    fn from(field: String) -> Self {
        Self::Field(field)
    }
}

/// Parse a provider stage for the hosted model try route with the call's catalogue rules.
pub fn provider_check_stage(task: &str, input: &Value) -> Option<SpeechStage> {
    parse_stage(task, input)
        .ok()
        .filter(|stage| !matches!(stage.place.as_str(), "device" | "host"))
}

pub(super) fn parse_stage(task: &str, input: &Value) -> Result<SpeechStage, StageFailure> {
    if !matches!(task, "stt" | "tts") {
        return Err("stage".to_owned().into());
    }
    let object = input.as_object().ok_or_else(|| "stage".to_owned())?;
    for key in object.keys() {
        if !matches!(key.as_str(), "place" | "model" | "options" | "build") {
            return Err(key.clone().into());
        }
    }
    let place = required_limited_string(object, "place", 60)?;
    let model_id = required_limited_string(object, "model", 120)?;
    if !valid_model_id(&model_id) {
        return Err("model".to_owned().into());
    }
    let build = match object.get("build") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_build(value).map_err(|()| "build".to_owned())?),
    };
    let given = match object.get("options") {
        None => Map::new(),
        Some(Value::Object(options)) => options.clone(),
        Some(_) => return Err("options".to_owned().into()),
    };

    let (model, schema) = if matches!(place.as_str(), "device" | "host") {
        let model = catalogue_model(task, &model_id, build.as_ref())?;
        (Some(model), model_schema(model))
    } else {
        provider_serves(task, &place)?;
        if build.is_some() {
            return Err("build".to_owned().into());
        }
        (None, provider_schema(&place, task))
    };

    let options = validate_options(schema, &given, model).map_err(StageFailure::Option)?;
    Ok(SpeechStage {
        place,
        model: model_id,
        options,
        build,
    })
}

/// The catalogue model a device or host stage names, which must serve `task` and list the chosen build.
pub(super) fn catalogue_model(
    task: &str,
    model_id: &str,
    build: Option<&ModelBuild>,
) -> Result<&'static Value, StageFailure> {
    let Some(model) = find_model(model_id) else {
        return Err("model".to_owned().into());
    };
    if task_for_model(model) != Some(task) {
        return Err("model".to_owned().into());
    }
    if let Some(build) = build {
        let listed = model
            .get("builds")
            .into_iter()
            .flat_map(values)
            .any(|candidate| field_str(candidate, "engine") == Some(build.engine.as_str()));
        if !listed {
            return Err("build.engine".to_owned().into());
        }
    }
    Ok(model)
}

/// A provider stage's place must be a catalogue provider that serves `task`.
pub(super) fn provider_serves(task: &str, place: &str) -> Result<(), StageFailure> {
    let Some(provider) = find_provider(place) else {
        return Err("place".to_owned().into());
    };
    if !strings(provider.get("tasks")).any(|candidate| candidate == task) {
        return Err("place".to_owned().into());
    }
    Ok(())
}

fn required_limited_string(
    object: &Map<String, Value>,
    field: &str,
    max: usize,
) -> Result<String, String> {
    match object.get(field).and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.chars().count() <= max => Ok(value.to_owned()),
        _ => Err(field.to_owned()),
    }
}

fn parse_build(value: &Value) -> Result<ModelBuild, ()> {
    let object = value.as_object().ok_or(())?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "engine" | "accelerator"))
    {
        return Err(());
    }
    let engine = required_limited_string(object, "engine", 60).map_err(|_| ())?;
    let accelerator = required_limited_string(object, "accelerator", 40).map_err(|_| ())?;
    Ok(ModelBuild {
        engine,
        accelerator,
    })
}

pub(super) fn valid_model_id(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    value.chars().count() <= 120
        && first.is_ascii_alphanumeric()
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'))
}
