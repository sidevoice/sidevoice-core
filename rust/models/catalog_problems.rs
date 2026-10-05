//! Soundness rules for a model catalogue: engines, ranking, families, providers, models and builds.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::json::{field_str, names, strings, values};

type Engines<'a> = HashMap<String, &'a Value>;

/// Validate the catalogue's engines, schemas, providers, models and build metadata.
pub fn catalog_problems(catalog: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    if catalog.get("version").and_then(Value::as_u64) != Some(2) {
        problems.push("version must be 2".to_owned());
    }
    let engines = engine_problems(catalog, &mut problems);
    for name in strings(
        catalog
            .get("ranking")
            .and_then(|ranking| ranking.get("default")),
    ) {
        if !engines.contains_key(name) {
            problems.push(format!("ranking: unknown engine {name}"));
        }
    }
    let families = catalog.get("families").and_then(Value::as_object);
    if let Some(families) = families {
        family_problems(families, &mut problems);
    }
    provider_problems(catalog, &mut problems);
    let mut model_ids = HashSet::new();
    for model in catalog.get("models").into_iter().flat_map(values) {
        model_problems(model, &engines, families, &mut model_ids, &mut problems);
    }
    problems
}

/// Check every engine and return them by id, the last listing winning as before.
fn engine_problems<'a>(catalog: &'a Value, problems: &mut Vec<String>) -> Engines<'a> {
    let mut engines = Engines::new();
    for engine in catalog.get("engines").into_iter().flat_map(values) {
        let name = field_str(engine, "id").unwrap_or("None").to_owned();
        if engines.insert(name.clone(), engine).is_some() {
            problems.push(format!("{name}: engine listed twice"));
        }
        let runs = field_str(engine, "runs");
        if !matches!(runs, Some("native" | "page")) {
            problems.push(format!("{name}: runs must be one of native, page"));
        }
        if runs == Some("native") {
            let packages = engine.get("packages").map(values).unwrap_or(&[]);
            if packages.is_empty() {
                problems.push(format!("{name}: native engine without packages"));
            }
            for package in packages {
                package_problems(&name, package, problems);
            }
        } else if runs == Some("page") && strings(engine.get("accelerators")).next().is_none() {
            problems.push(format!("{name}: a page engine lists no accelerators"));
        }
    }
    engines
}

fn package_problems(engine: &str, package: &Value, problems: &mut Vec<String>) {
    let where_ = format!(
        "{engine} {}/{}",
        field_str(package, "os").unwrap_or("None"),
        field_str(package, "arch").unwrap_or("*")
    );
    if strings(package.get("accelerators")).next().is_none() {
        problems.push(format!("{where_}: a package lists no accelerators"));
    }
    if package.get("bundled").and_then(Value::as_bool) == Some(true) {
        return;
    }
    if field_str(package, "arch").is_none() {
        problems.push(format!(
            "{where_}: a downloaded package names its architecture"
        ));
    }
    if let Some(problem) = download_problem(package.get("download")) {
        problems.push(format!("{where_}: {problem}"));
    }
    for library in package.get("libraries").into_iter().flat_map(values) {
        let path = field_str(library, "path").unwrap_or("");
        if !valid_sha256(library.get("sha256"))
            || path.contains("..")
            || path.starts_with('/')
            || path.is_empty()
        {
            problems.push(format!(
                "{where_}: library {path} needs a sha256 and a relative path"
            ));
        }
    }
}

fn family_problems(families: &Map<String, Value>, problems: &mut Vec<String>) {
    for (name, family) in families {
        if !matches!(field_str(family, "task"), Some("stt" | "tts")) {
            problems.push(format!("{name}: family task must be one of stt, tts"));
        }
        problems.extend(option_problems(
            &format!("family {name}"),
            family.get("options"),
        ));
    }
}

fn provider_problems(catalog: &Value, problems: &mut Vec<String>) {
    let mut providers = HashSet::new();
    for provider in catalog.get("providers").into_iter().flat_map(values) {
        let name = field_str(provider, "id").unwrap_or("None");
        if !providers.insert(name.to_owned()) || matches!(name, "device" | "host") {
            problems.push(format!(
                "{name}: provider id must be unique and not a place"
            ));
        }
        for task in strings(provider.get("tasks")) {
            if !matches!(task, "stt" | "tts") {
                problems.push(format!("{name}: unknown task {task}"));
                continue;
            }
            let entry = provider.get(task);
            let has_models = entry.is_some_and(|entry| {
                matches!(entry.get("models"), Some(Value::String(mode)) if mode == "remote")
                    || entry.get("models").is_some_and(Value::is_array)
            });
            if !has_models {
                problems.push(format!("{name}: {task} needs models, a list or \"remote\""));
                continue;
            }
            problems.extend(option_problems(
                &format!("provider {name} {task}"),
                entry.and_then(|v| v.get("options")),
            ));
        }
    }
}

fn model_problems(
    model: &Value,
    engines: &Engines<'_>,
    families: Option<&Map<String, Value>>,
    model_ids: &mut HashSet<String>,
    problems: &mut Vec<String>,
) {
    let name = field_str(model, "id").unwrap_or("None");
    if !model_ids.insert(name.to_owned()) {
        problems.push(format!("{name}: model listed twice"));
    }
    let family_name = field_str(model, "family").unwrap_or("None");
    let Some(family) = families.and_then(|families| families.get(family_name)) else {
        problems.push(format!("{name}: unknown family {family_name}"));
        return;
    };
    let memory = model.get("requires").and_then(|v| v.get("memory_mb"));
    if memory.is_some_and(|memory| memory.as_u64().is_none_or(|number| number == 0)) {
        problems.push(format!(
            "{name}: requires.memory_mb must be a positive whole number"
        ));
    }
    let mut on = HashSet::new();
    for build in model.get("builds").into_iter().flat_map(values) {
        build_problems(name, family_name, build, engines, &mut on, problems);
    }
    if values(model.get("builds").unwrap_or(&Value::Null)).is_empty() {
        problems.push(format!("{name}: no builds"));
    }
    let voices = model.get("voices").map(values).unwrap_or(&[]);
    let voice_ids = voices
        .iter()
        .filter_map(|voice| field_str(voice, "id"))
        .collect::<Vec<_>>();
    if values(family.get("options").unwrap_or(&Value::Null))
        .iter()
        .any(|option| field_str(option, "from") == Some("model.voices"))
        && voice_ids.is_empty()
    {
        problems.push(format!("{name}: a voice model lists no voices"));
    }
    if voice_ids.iter().collect::<HashSet<_>>().len() != voice_ids.len() {
        problems.push(format!("{name}: a voice listed twice"));
    }
}

fn build_problems(
    name: &str,
    family_name: &str,
    build: &Value,
    engines: &Engines<'_>,
    on: &mut HashSet<String>,
    problems: &mut Vec<String>,
) {
    let engine_id = field_str(build, "engine").unwrap_or("None");
    let Some(engine) = engines.get(engine_id).copied() else {
        problems.push(format!("{name}: unknown engine {engine_id}"));
        return;
    };
    if !on.insert(engine_id.to_owned()) {
        problems.push(format!("{name}: two builds on {engine_id}"));
    }
    if !names(engine.get("families")).contains(family_name) {
        problems.push(format!(
            "{name} on {engine_id}: the engine does not run the {family_name} family"
        ));
    }
    let format = field_str(build, "format").unwrap_or("None");
    if !names(engine.get("formats")).contains(format) {
        problems.push(format!("{name} on {engine_id}: format {format} not read"));
    }
    if field_str(engine, "runs") == Some("native") {
        if let Some(problem) = download_problem(build.get("download")) {
            problems.push(format!("{name} on {engine_id}: native build {problem}"));
        }
    }
    let mut usable = names(engine.get("accelerators"));
    for package in engine.get("packages").into_iter().flat_map(values) {
        usable.extend(names(package.get("accelerators")));
    }
    if !names(build.get("accelerators")).is_subset(&usable) {
        problems.push(format!(
            "{name} on {engine_id}: an accelerator the engine never uses"
        ));
    }
}

fn option_problems(where_: &str, schema: Option<&Value>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = HashSet::new();
    for option in schema.into_iter().flat_map(values) {
        let name = field_str(option, "id").unwrap_or("");
        if name.is_empty() || !seen.insert(name.to_owned()) {
            problems.push(format!(
                "{where_}: option ids must be present and unique ({name:?})"
            ));
        }
        let kind = field_str(option, "kind").unwrap_or("None");
        if !matches!(kind, "language" | "text" | "voice" | "range") {
            problems.push(format!(
                "{where_}: option {name} has an unknown kind {kind:?}"
            ));
        }
        if kind == "range" {
            let low = option.get("min").and_then(Value::as_f64);
            let high = option.get("max").and_then(Value::as_f64);
            let default = option.get("default").and_then(Value::as_f64);
            if !matches!((low, high), (Some(low), Some(high)) if low < high) {
                problems.push(format!("{where_}: range {name} needs min < max"));
            } else if let (Some(default), Some(low), Some(high)) = (default, low, high) {
                if !(low..=high).contains(&default) {
                    problems.push(format!("{where_}: range {name} has its default outside it"));
                }
            }
        }
        if kind == "text" && option.get("max").is_some() {
            let max = option.get("max");
            if max.and_then(Value::as_u64).is_none_or(|limit| limit == 0) {
                problems.push(format!("{where_}: text {name} needs a positive whole max"));
            }
        }
    }
    problems
}

fn valid_sha256(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn download_problem(download: Option<&Value>) -> Option<&'static str> {
    let Some(download) = download.filter(|download| download.is_object()) else {
        return Some("needs an https download with sha256");
    };
    if !field_str(download, "url").is_some_and(|url| url.starts_with("https://"))
        || !valid_sha256(download.get("sha256"))
    {
        return Some("needs an https download with sha256");
    }
    if download
        .get("size")
        .and_then(Value::as_u64)
        .is_none_or(|size| size == 0)
    {
        return Some("download needs its size");
    }
    None
}
