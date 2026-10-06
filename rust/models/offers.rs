//! The resolver: which build of each catalogue model runs on a device, and why that one.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::json::{contains_all, field_str, names, strings, values};

/// A resolver place other than a known provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownPlace(pub String);

impl fmt::Display for UnknownPlace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown place {:?}", self.0)
    }
}

impl Error for UnknownPlace {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuildAlternative {
    pub engine: String,
    pub accelerator: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelOffer {
    pub model: String,
    pub task: String,
    pub engine: String,
    pub accelerator: String,
    pub download_size: u64,
    pub reason: String,
    pub alternatives: Vec<BuildAlternative>,
}

/// What the device reported, read once for every model.
struct Device<'a> {
    capabilities: &'a Value,
    runs: Option<&'a str>,
    platform: String,
    has: HashSet<String>,
    memory: Option<f64>,
}

/// A build that runs on the device, with the accelerators it may use there.
struct Fitting<'a> {
    index: usize,
    build: &'a Value,
    accelerators: Vec<String>,
    package: Option<&'a Value>,
}

/// One resolver result per model, in catalogue order, matching `vectors.json` byte-for-byte as data.
pub fn offers(
    catalog: &Value,
    capabilities: &Value,
    place: &str,
) -> Result<Vec<ModelOffer>, UnknownPlace> {
    if !matches!(place, "device" | "host") {
        let is_provider = catalog
            .get("providers")
            .into_iter()
            .flat_map(values)
            .any(|provider| field_str(provider, "id") == Some(place));
        return if is_provider {
            Ok(Vec::new())
        } else {
            Err(UnknownPlace(place.to_owned()))
        };
    }

    let engines = catalog
        .get("engines")
        .into_iter()
        .flat_map(values)
        .filter_map(|engine| field_str(engine, "id").map(|id| (id, engine)))
        .collect::<HashMap<_, _>>();
    let engine_order = strings(
        catalog
            .get("ranking")
            .and_then(|ranking| ranking.get("default")),
    )
    .collect::<Vec<_>>();
    let device = Device::from(capabilities);

    let mut result = Vec::new();
    for model in catalog.get("models").into_iter().flat_map(values) {
        let needed_memory = model
            .get("requires")
            .and_then(|requires| requires.get("memory_mb"))
            .and_then(Value::as_f64);
        if matches!((needed_memory, device.memory), (Some(needed), Some(memory)) if memory < needed)
        {
            continue;
        }
        let mut fitting = fitting_builds(model, &engines, &device);
        if fitting.is_empty() {
            continue;
        }
        fitting.sort_by_key(|candidate| rank_key(candidate, &device.platform, &engine_order));
        result.push(offer(catalog, model, &fitting, &device.platform));
    }
    Ok(result)
}

impl<'a> From<&'a Value> for Device<'a> {
    fn from(capabilities: &'a Value) -> Self {
        let runs = field_str(capabilities, "runs");
        let platform = if runs == Some("native") {
            format!(
                "{}-{}",
                field_str(capabilities, "os").unwrap_or("None"),
                field_str(capabilities, "arch").unwrap_or("None")
            )
        } else {
            "page".to_owned()
        };
        Self {
            capabilities,
            runs,
            platform,
            has: names(capabilities.get("has")),
            memory: capabilities.get("memory_mb").and_then(Value::as_f64),
        }
    }
}

/// The model's builds that run on the device, in catalogue order.
fn fitting_builds<'a>(
    model: &'a Value,
    engines: &HashMap<&str, &'a Value>,
    device: &Device<'_>,
) -> Vec<Fitting<'a>> {
    let mut fitting = Vec::new();
    for (index, build) in model.get("builds").into_iter().flat_map(values).enumerate() {
        let Some(engine) = field_str(build, "engine")
            .and_then(|id| engines.get(id))
            .copied()
        else {
            continue;
        };
        if field_str(engine, "runs") != device.runs {
            continue;
        }
        let package = if device.runs == Some("native") {
            let Some(package) = package_for(engine, device.capabilities, &device.has) else {
                continue;
            };
            Some(package)
        } else {
            None
        };
        if !contains_all(build.get("needs"), &device.has) {
            continue;
        }
        let usable = package
            .and_then(|package| package.get("accelerators"))
            .or_else(|| engine.get("accelerators"));
        let listed = build
            .get("accelerators")
            .unwrap_or(usable.unwrap_or(&Value::Null));
        let accelerators = strings(Some(listed))
            .filter(|accelerator| {
                strings(usable).any(|candidate| candidate == *accelerator)
                    && device.has.contains(*accelerator)
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !accelerators.is_empty() {
            fitting.push(Fitting {
                index,
                build,
                accelerators,
                package,
            });
        }
    }
    fitting
}

/// Builds the model ranks for this platform come first, then the catalogue's engine order, then build order.
fn rank_key(
    candidate: &Fitting<'_>,
    platform: &str,
    engine_order: &[&str],
) -> (bool, i64, usize, usize) {
    let rank = candidate
        .build
        .get("rank")
        .and_then(|rank| rank.get(platform))
        .and_then(Value::as_i64);
    let engine = field_str(candidate.build, "engine").unwrap_or("");
    (
        rank.is_none(),
        rank.unwrap_or(0),
        engine_order
            .iter()
            .position(|item| *item == engine)
            .unwrap_or(engine_order.len()),
        candidate.index,
    )
}

/// The offer for the best of the ranked `fitting` builds, listing every other usable pairing.
fn offer(catalog: &Value, model: &Value, fitting: &[Fitting<'_>], platform: &str) -> ModelOffer {
    let best = &fitting[0];
    let model_id = field_str(model, "id").unwrap_or("");
    let family = field_str(model, "family").and_then(|family| catalog.get("families")?.get(family));
    let task = family
        .and_then(|family| field_str(family, "task"))
        .unwrap_or("");
    let engine = field_str(best.build, "engine").unwrap_or("");
    let accelerator = best
        .accelerators
        .first()
        .expect("a fitting build has at least one accelerator");
    let rank = best.build.get("rank").and_then(|rank| rank.get(platform));
    let reason = if fitting.len() == 1 {
        "the only build that runs here".to_owned()
    } else if rank.is_some() && !rank.is_some_and(Value::is_null) {
        format!("this model ranks it first on {platform}")
    } else {
        "first in the catalogue's engine order".to_owned()
    };
    let size = best
        .package
        .filter(|package| package.get("bundled").and_then(Value::as_bool) != Some(true))
        .and_then(|package| package.get("download"))
        .and_then(|download| download.get("size"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(
            best.build
                .get("download")
                .and_then(|download| download.get("size"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    let alternatives = fitting
        .iter()
        .flat_map(|candidate| {
            candidate
                .accelerators
                .iter()
                .map(move |accelerator| BuildAlternative {
                    engine: field_str(candidate.build, "engine")
                        .unwrap_or("")
                        .to_owned(),
                    accelerator: accelerator.clone(),
                })
        })
        .skip(1)
        .collect();
    ModelOffer {
        model: model_id.to_owned(),
        task: task.to_owned(),
        engine: engine.to_owned(),
        accelerator: accelerator.clone(),
        download_size: size,
        reason: format!("{engine} ({accelerator}): {reason}"),
        alternatives,
    }
}

fn package_for<'a>(
    engine: &'a Value,
    capabilities: &Value,
    has: &HashSet<String>,
) -> Option<&'a Value> {
    engine
        .get("packages")
        .into_iter()
        .flat_map(values)
        .find(|package| {
            package.get("os") == capabilities.get("os")
                && (package.get("arch").is_none_or(Value::is_null)
                    || package.get("arch") == capabilities.get("arch"))
                && contains_all(package.get("requires"), has)
        })
}
