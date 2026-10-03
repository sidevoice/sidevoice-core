//! Catalogue, settings and model-check rules shared with the existing Python vectors.

use std::{collections::HashSet, error::Error, fmt, sync::OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

use crate::{
    messages::{render, ui_locale, LocalizedMessage},
    types::{CallSettings, ModelBuild, SpeechStage},
};

const CATALOG_JSON: &str = include_str!("../src/sidevoice_core/models/catalog.json");
#[cfg(test)]
const VECTORS_JSON: &str = include_str!("../src/sidevoice_core/models/vectors.json");
const VOICE_CATALOG_JSON: &str = include_str!("../src/sidevoice_core/pipeline/catalog.json");
const CHECKS_JSON: &str = include_str!("../src/sidevoice_core/models/checks/checks.json");
const CHECK_ES_WAV: &[u8] = include_bytes!("../src/sidevoice_core/models/checks/stt-es.wav");
const CHECK_EN_WAV: &[u8] = include_bytes!("../src/sidevoice_core/models/checks/stt-en.wav");

static CATALOG: OnceLock<Value> = OnceLock::new();
static VOICE_CATALOG: OnceLock<Value> = OnceLock::new();
static CHECKS: OnceLock<Value> = OnceLock::new();
static EMPTY_NULL: OnceLock<Value> = OnceLock::new();

/// The exact catalogue bytes served to existing clients.
pub fn catalog_text() -> &'static str {
    CATALOG_JSON
}

/// The one model catalogue used for validation and resolution.
pub fn catalog() -> &'static Value {
    CATALOG.get_or_init(|| {
        serde_json::from_str(CATALOG_JSON).expect("embedded model catalogue is valid JSON")
    })
}

fn voice_catalog() -> &'static Value {
    VOICE_CATALOG.get_or_init(|| {
        serde_json::from_str(VOICE_CATALOG_JSON).expect("embedded voice catalogue is valid JSON")
    })
}

fn checks() -> &'static Value {
    CHECKS.get_or_init(|| {
        serde_json::from_str(CHECKS_JSON).expect("embedded check rules are valid JSON")
    })
}

fn null_value() -> &'static Value {
    EMPTY_NULL.get_or_init(|| Value::Null)
}

fn values(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn strings(value: Option<&Value>) -> impl Iterator<Item = &str> {
    value.into_iter().flat_map(values).filter_map(Value::as_str)
}

fn field_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn names(value: Option<&Value>) -> HashSet<String> {
    strings(value).map(str::to_owned).collect()
}

fn contains_all(required: Option<&Value>, available: &HashSet<String>) -> bool {
    strings(required).all(|item| available.contains(item))
}

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

/// Validate the catalogue's engines, schemas, providers, models and build metadata.
pub fn catalog_problems(catalog: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    if catalog.get("version").and_then(Value::as_u64) != Some(2) {
        problems.push("version must be 2".to_owned());
    }

    let mut engines = std::collections::HashMap::<String, &Value>::new();
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
                let where_ = format!(
                    "{name} {}/{}",
                    field_str(package, "os").unwrap_or("None"),
                    field_str(package, "arch").unwrap_or("*")
                );
                if strings(package.get("accelerators")).next().is_none() {
                    problems.push(format!("{where_}: a package lists no accelerators"));
                }
                if package.get("bundled").and_then(Value::as_bool) == Some(true) {
                    continue;
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
        } else if runs == Some("page") && strings(engine.get("accelerators")).next().is_none() {
            problems.push(format!("{name}: a page engine lists no accelerators"));
        }
    }
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

    let mut model_ids = HashSet::new();
    for model in catalog.get("models").into_iter().flat_map(values) {
        let name = field_str(model, "id").unwrap_or("None");
        if !model_ids.insert(name.to_owned()) {
            problems.push(format!("{name}: model listed twice"));
        }
        let family_name = field_str(model, "family").unwrap_or("None");
        let family = families.and_then(|families| families.get(family_name));
        let Some(family) = family else {
            problems.push(format!("{name}: unknown family {family_name}"));
            continue;
        };
        let memory = model.get("requires").and_then(|v| v.get("memory_mb"));
        if memory.is_some_and(|memory| memory.as_u64().is_none_or(|number| number == 0)) {
            problems.push(format!(
                "{name}: requires.memory_mb must be a positive whole number"
            ));
        }
        let mut on = HashSet::new();
        for build in model.get("builds").into_iter().flat_map(values) {
            let engine_id = field_str(build, "engine").unwrap_or("None");
            let Some(engine) = engines.get(engine_id).copied() else {
                problems.push(format!("{name}: unknown engine {engine_id}"));
                continue;
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
    problems
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
        .collect::<std::collections::HashMap<_, _>>();
    let engine_order = strings(
        catalog
            .get("ranking")
            .and_then(|ranking| ranking.get("default")),
    )
    .collect::<Vec<_>>();
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
    let has = names(capabilities.get("has"));
    let memory = capabilities.get("memory_mb").and_then(Value::as_f64);
    let mut result = Vec::new();

    for model in catalog.get("models").into_iter().flat_map(values) {
        let needed_memory = model
            .get("requires")
            .and_then(|requires| requires.get("memory_mb"))
            .and_then(Value::as_f64);
        if matches!((needed_memory, memory), (Some(needed), Some(memory)) if memory < needed) {
            continue;
        }
        let mut fitting = Vec::<(usize, &Value, Vec<String>, Option<&Value>)>::new();
        for (index, build) in model.get("builds").into_iter().flat_map(values).enumerate() {
            let Some(engine) = field_str(build, "engine")
                .and_then(|id| engines.get(id))
                .copied()
            else {
                continue;
            };
            if field_str(engine, "runs") != runs {
                continue;
            }
            let package = if runs == Some("native") {
                let Some(package) = package_for(engine, capabilities, &has) else {
                    continue;
                };
                Some(package)
            } else {
                None
            };
            if !contains_all(build.get("needs"), &has) {
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
                        && has.contains(*accelerator)
                })
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !accelerators.is_empty() {
                fitting.push((index, build, accelerators, package));
            }
        }
        if fitting.is_empty() {
            continue;
        }
        fitting.sort_by(|left, right| {
            let build_rank = |build: &Value| {
                build
                    .get("rank")
                    .and_then(|rank| rank.get(&platform))
                    .and_then(Value::as_i64)
            };
            let (left_index, left_build, _, _) = left;
            let (right_index, right_build, _, _) = right;
            let left_rank = build_rank(left_build);
            let right_rank = build_rank(right_build);
            let left_engine = field_str(left_build, "engine").unwrap_or("");
            let right_engine = field_str(right_build, "engine").unwrap_or("");
            let rank_key = |rank: Option<i64>, engine: &str, index: usize| {
                (
                    rank.is_none(),
                    rank.unwrap_or(0),
                    engine_order
                        .iter()
                        .position(|item| *item == engine)
                        .unwrap_or(engine_order.len()),
                    index,
                )
            };
            rank_key(left_rank, left_engine, *left_index).cmp(&rank_key(
                right_rank,
                right_engine,
                *right_index,
            ))
        });

        let (_, best, accelerators, package) = &fitting[0];
        let package = *package;
        let model_id = field_str(model, "id").unwrap_or("");
        let family =
            field_str(model, "family").and_then(|family| catalog.get("families")?.get(family));
        let task = family
            .and_then(|family| field_str(family, "task"))
            .unwrap_or("");
        let engine = field_str(best, "engine").unwrap_or("");
        let accelerator = accelerators
            .first()
            .expect("a fitting build has at least one accelerator");
        let rank = best.get("rank").and_then(|rank| rank.get(&platform));
        let reason = if fitting.len() == 1 {
            "the only build that runs here".to_owned()
        } else if rank.is_some() && !rank.is_some_and(Value::is_null) {
            format!("this model ranks it first on {platform}")
        } else {
            "first in the catalogue's engine order".to_owned()
        };
        let size = package
            .filter(|package| package.get("bundled").and_then(Value::as_bool) != Some(true))
            .and_then(|package| package.get("download"))
            .and_then(|download| download.get("size"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(
                best.get("download")
                    .and_then(|download| download.get("size"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
        let alternatives = fitting
            .iter()
            .flat_map(|(_, build, accelerators, _)| {
                accelerators
                    .iter()
                    .map(move |accelerator| BuildAlternative {
                        engine: field_str(build, "engine").unwrap_or("").to_owned(),
                        accelerator: accelerator.clone(),
                    })
            })
            .skip(1)
            .collect();
        result.push(ModelOffer {
            model: model_id.to_owned(),
            task: task.to_owned(),
            engine: engine.to_owned(),
            accelerator: accelerator.clone(),
            download_size: size,
            reason: format!("{engine} ({accelerator}): {reason}"),
            alternatives,
        });
    }
    Ok(result)
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

#[derive(Clone, Debug)]
pub struct SettingsLoad {
    pub settings: CallSettings,
    /// Existing clients receive the rendered reason string, not an internal message object.
    pub issue: Option<String>,
}

#[derive(Clone, Debug)]
struct SettingDiagnostic {
    path: String,
    message: LocalizedMessage,
}

impl SettingDiagnostic {
    fn new(path: impl Into<String>, message: LocalizedMessage) -> Self {
        Self {
            path: path.into(),
            message,
        }
    }
}

#[derive(Clone, Debug)]
enum StageFailure {
    Field(String),
    Option(OptionFailure),
}

impl From<String> for StageFailure {
    fn from(field: String) -> Self {
        Self::Field(field)
    }
}

#[derive(Clone, Debug)]
struct OptionFailure {
    name: String,
    kind: OptionFailureKind,
}

#[derive(Clone, Debug)]
enum OptionFailureKind {
    Unknown,
    Language { shown: String },
    TextInvalid { max: usize },
    Range(Box<(Value, Value)>),
    VoiceShape,
    VoiceSpeechLanguage { shown: String },
    ModelVoice { shown: String },
    VoiceLanguage { shown: String, language: String },
    ProviderVoice,
    Other,
}

impl OptionFailure {
    fn message(&self) -> LocalizedMessage {
        match &self.kind {
            OptionFailureKind::Unknown => LocalizedMessage::new("settings.option_unknown")
                .with_param("name", self.name.clone()),
            OptionFailureKind::Language { shown } => {
                LocalizedMessage::new("settings.option_language_invalid")
                    .with_param("name", self.name.clone())
                    .with_param("value", shown.clone())
            }
            OptionFailureKind::TextInvalid { max } => {
                LocalizedMessage::new("settings.option_text_too_long")
                    .with_param("name", self.name.clone())
                    .with_param("max", *max)
            }
            OptionFailureKind::Range(bounds) => LocalizedMessage::new("settings.stage_range")
                .with_param("name", self.name.clone())
                .with_param("min", bounds.0.clone())
                .with_param("max", bounds.1.clone()),
            OptionFailureKind::VoiceShape => LocalizedMessage::new("settings.option_voice_shape")
                .with_param("name", self.name.clone())
                .with_param("example", "{language: voice}"),
            OptionFailureKind::VoiceSpeechLanguage { shown } => {
                LocalizedMessage::new("settings.option_voice_speech_language")
                    .with_param("name", self.name.clone())
                    .with_param("value", shown.clone())
            }
            OptionFailureKind::ModelVoice { shown } => {
                LocalizedMessage::new("settings.option_model_voice_invalid")
                    .with_param("name", self.name.clone())
                    .with_param("value", shown.clone())
            }
            OptionFailureKind::VoiceLanguage { shown, language } => {
                LocalizedMessage::new("settings.option_voice_language")
                    .with_param("name", self.name.clone())
                    .with_param("voice", shown.clone())
                    .with_param("language", language.clone())
            }
            OptionFailureKind::ProviderVoice => {
                LocalizedMessage::new("settings.option_provider_voice_invalid")
                    .with_param("name", self.name.clone())
                    .with_param("max", 120)
            }
            OptionFailureKind::Other => LocalizedMessage::new("settings.stage_invalid"),
        }
    }

    fn new(name: impl Into<String>, kind: OptionFailureKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CredentialState {
    pub configured: bool,
    pub source: Option<&'static str>,
    pub hint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MicSettings {
    pub turn_end_mode: String,
    pub user_speech_timeout: f32,
    pub smart_turn_min_silence: f32,
    pub smart_turn_max_silence: f32,
    pub vad_confidence: f32,
    pub vad_min_volume: f32,
    pub vad_start_secs: f32,
    pub merge_window_secs: f32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ResolvedVoice {
    pub place: String,
    pub model: String,
    pub voice: String,
    pub language: String,
    pub speed: f64,
}

/// Build settings defaults from the device's reported capabilities and language.
/// UI locales are only `en` and `es`; speech-language options retain the catalogue's wider set.
pub fn default_settings(
    system_language: Option<&str>,
    device_capabilities: Option<&Value>,
) -> CallSettings {
    let speech_language = system_language.map(normalize_speech_language);
    let ui_language = system_language.map(ui_locale).unwrap_or("en").to_owned();
    let stt = default_stage("stt", device_capabilities, speech_language.as_deref());
    let tts = default_stage("tts", device_capabilities, speech_language.as_deref());
    CallSettings {
        stt,
        tts,
        ui_language,
        turn_patience: "normal".to_owned(),
        turn_end_mode: "smart_turn".to_owned(),
        user_speech_timeout: 2.5,
        smart_turn_min_silence: 0.9,
        smart_turn_max_silence: 3.0,
        vad_confidence: 0.6,
        vad_min_volume: 0.5,
        vad_start_secs: 0.4,
        merge_window_secs: 0.5,
        audio_grace_seconds: 1.0,
        replay_on_return_seconds: 120.0,
    }
}

fn normalize_speech_language(tag: &str) -> String {
    let primary = tag.split('-').next().unwrap_or("").to_ascii_lowercase();
    let supported = voice_catalog()
        .get("languages")
        .into_iter()
        .flat_map(values)
        .filter_map(|entry| field_str(entry, "id"))
        .any(|language| language == primary);
    if supported {
        primary
    } else {
        "en".to_owned()
    }
}

fn default_stage(task: &str, capabilities: Option<&Value>, language: Option<&str>) -> SpeechStage {
    let chosen = capabilities
        .and_then(|capabilities| offers(catalog(), capabilities, "device").ok())
        .and_then(|offers| offers.into_iter().find(|offer| offer.task == task))
        .map(|offer| offer.model);
    let model = chosen
        .or_else(|| {
            catalog()
                .get("models")
                .into_iter()
                .flat_map(values)
                .find(|model| task_for_model(model) == Some(task))
                .and_then(|model| field_str(model, "id"))
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let model_entry = find_model(&model).expect("default model exists in embedded catalogue");
    let schema = model_schema(model_entry);
    let mut options = Map::new();
    for option in values(schema) {
        let id = field_str(option, "id").unwrap_or("");
        if field_str(option, "kind") == Some("language")
            && language.is_some_and(|language| {
                strings(option.get("values")).any(|candidate| candidate == language)
            })
        {
            options.insert(id.to_owned(), Value::String(language.unwrap().to_owned()));
        } else if let Some(default) = option.get("default") {
            options.insert(id.to_owned(), normalized_default(option, default));
        }
    }
    SpeechStage {
        place: "device".to_owned(),
        model,
        options,
        build: None,
    }
}

fn task_for_model(model: &Value) -> Option<&str> {
    field_str(model, "family")
        .and_then(|family| catalog().get("families")?.get(family))
        .and_then(|family| field_str(family, "task"))
}

fn find_model(model_id: &str) -> Option<&'static Value> {
    catalog()
        .get("models")
        .into_iter()
        .flat_map(values)
        .find(|model| field_str(model, "id") == Some(model_id))
}

fn find_provider(provider_id: &str) -> Option<&'static Value> {
    catalog()
        .get("providers")
        .into_iter()
        .flat_map(values)
        .find(|provider| field_str(provider, "id") == Some(provider_id))
}

fn model_schema(model: &Value) -> &'static Value {
    let family = field_str(model, "family").unwrap_or("");
    catalog()
        .get("families")
        .and_then(|families| families.get(family))
        .and_then(|family| family.get("options"))
        .unwrap_or_else(|| null_value())
}

fn normalized_default(option: &Value, default: &Value) -> Value {
    if field_str(option, "kind") == Some("range") {
        default
            .as_f64()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| default.clone())
    } else {
        default.clone()
    }
}

fn task_schema<'a>(provider: &'a Value, task: &str) -> Option<&'a Value> {
    provider.get(task).and_then(|entry| entry.get("options"))
}

fn speech_catalogue_language(language: &str) -> bool {
    voice_catalog()
        .get("languages")
        .into_iter()
        .flat_map(values)
        .any(|entry| field_str(entry, "id") == Some(language))
}

/// Validate incoming settings, falling back only the fields that failed and whole stages as one field.
pub fn settings_from(input: Option<&Value>, defaults: &CallSettings) -> SettingsLoad {
    let mut settings = defaults.clone();
    let Some(object) = input
        .and_then(Value::as_object)
        .filter(|object| !object.is_empty())
    else {
        return SettingsLoad {
            settings,
            issue: None,
        };
    };
    let mut invalid = Vec::<SettingDiagnostic>::new();

    if let Some(value) = object.get("ui_language") {
        match value.as_str() {
            Some("en") => settings.ui_language = "en".to_owned(),
            Some("es") => settings.ui_language = "es".to_owned(),
            _ => invalid.push(SettingDiagnostic::new(
                "ui_language",
                LocalizedMessage::new("settings.ui_language_invalid"),
            )),
        }
    }
    if let Some(value) = object.get("turn_patience") {
        match value.as_str() {
            Some(value @ ("fast" | "normal" | "calm")) => settings.turn_patience = value.to_owned(),
            _ => invalid.push(SettingDiagnostic::new(
                "turn_patience",
                LocalizedMessage::new("settings.turn_patience_invalid"),
            )),
        }
    }
    if let Some(value) = object.get("turn_end_mode") {
        match value.as_str() {
            Some(value @ ("timer" | "smart_turn")) => settings.turn_end_mode = value.to_owned(),
            _ => invalid.push(SettingDiagnostic::new(
                "turn_end_mode",
                LocalizedMessage::new("settings.turn_end_mode_invalid"),
            )),
        }
    }

    settings.audio_grace_seconds = float_field(
        object,
        "audio_grace_seconds",
        settings.audio_grace_seconds,
        0.0,
        10.0,
        &mut invalid,
    );
    settings.replay_on_return_seconds = float_field(
        object,
        "replay_on_return_seconds",
        settings.replay_on_return_seconds,
        0.0,
        3600.0,
        &mut invalid,
    );
    settings.user_speech_timeout = float_field(
        object,
        "user_speech_timeout",
        settings.user_speech_timeout,
        0.5,
        15.0,
        &mut invalid,
    );
    settings.smart_turn_min_silence = float_field(
        object,
        "smart_turn_min_silence",
        settings.smart_turn_min_silence,
        0.1,
        3.0,
        &mut invalid,
    );
    settings.smart_turn_max_silence = float_field(
        object,
        "smart_turn_max_silence",
        settings.smart_turn_max_silence,
        0.5,
        15.0,
        &mut invalid,
    );
    settings.vad_confidence = float_field(
        object,
        "vad_confidence",
        settings.vad_confidence,
        0.1,
        1.0,
        &mut invalid,
    );
    settings.vad_min_volume = float_field(
        object,
        "vad_min_volume",
        settings.vad_min_volume,
        0.0,
        1.0,
        &mut invalid,
    );
    settings.vad_start_secs = float_field(
        object,
        "vad_start_secs",
        settings.vad_start_secs,
        0.05,
        1.0,
        &mut invalid,
    );
    settings.merge_window_secs = float_field(
        object,
        "merge_window_secs",
        settings.merge_window_secs,
        0.0,
        5.0,
        &mut invalid,
    );

    for task in ["stt", "tts"] {
        if let Some(value) = object.get(task) {
            match parse_stage(task, value) {
                Ok(stage) => {
                    if task == "stt" {
                        settings.stt = stage
                    } else {
                        settings.tts = stage
                    }
                }
                Err(StageFailure::Field(field)) => {
                    invalid.extend(stage_diagnostics(task, &field, value))
                }
                Err(StageFailure::Option(option)) => {
                    invalid.push(SettingDiagnostic::new(task, option.message()))
                }
            }
        }
    }

    const FIELD_ORDER: [&str; 14] = [
        "ui_language",
        "stt",
        "tts",
        "audio_grace_seconds",
        "replay_on_return_seconds",
        "turn_patience",
        "turn_end_mode",
        "user_speech_timeout",
        "smart_turn_min_silence",
        "smart_turn_max_silence",
        "vad_confidence",
        "vad_min_volume",
        "vad_start_secs",
        "merge_window_secs",
    ];
    invalid.sort_by_key(|diagnostic| {
        let root = diagnostic
            .path
            .split('.')
            .next()
            .unwrap_or(&diagnostic.path);
        FIELD_ORDER
            .iter()
            .position(|candidate| *candidate == root)
            .unwrap_or(FIELD_ORDER.len())
    });
    let issue = if invalid.is_empty() {
        None
    } else {
        let details = invalid
            .iter()
            .take(3)
            .map(|diagnostic| {
                format!(
                    "{} {}",
                    diagnostic.path,
                    render(&diagnostic.message, &settings.ui_language)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        Some(render(
            &LocalizedMessage::new("settings.invalid").with_param("details", details),
            &settings.ui_language,
        ))
    };
    SettingsLoad { settings, issue }
}

fn float_field(
    object: &Map<String, Value>,
    name: &str,
    default: f32,
    min: f64,
    max: f64,
    invalid: &mut Vec<SettingDiagnostic>,
) -> f32 {
    let Some(value) = object.get(name) else {
        return default;
    };
    match numeric_value(value) {
        Some(value) if value >= min && value <= max => value as f32,
        Some(value) if value < min => {
            invalid.push(SettingDiagnostic::new(
                name,
                LocalizedMessage::new("settings.input_greater_equal")
                    .with_param("bound", min.to_string()),
            ));
            default
        }
        Some(_) => {
            invalid.push(SettingDiagnostic::new(
                name,
                LocalizedMessage::new("settings.input_less_equal")
                    .with_param("bound", max.to_string()),
            ));
            default
        }
        _ => {
            let key = if object.get(name).is_some_and(Value::is_string) {
                "settings.input_number_parse"
            } else {
                "settings.input_number"
            };
            invalid.push(SettingDiagnostic::new(name, LocalizedMessage::new(key)));
            default
        }
    }
}

fn stage_diagnostics(task: &str, field: &str, input: &Value) -> Vec<SettingDiagnostic> {
    if let Some(object) = input.as_object() {
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
            return missing;
        }

        if !matches!(
            field,
            "place" | "model" | "build" | "build.engine" | "options"
        ) && !field.starts_with("options.")
        {
            return vec![SettingDiagnostic::new(
                format!("{task}.{field}"),
                LocalizedMessage::new("settings.extra_input"),
            )];
        }

        if field == "model" {
            if let Some(model) = object.get("model").and_then(Value::as_str) {
                if !valid_model_id(model) {
                    return vec![SettingDiagnostic::new(
                        task,
                        LocalizedMessage::new("settings.model_id_invalid")
                            .with_param("model", model),
                    )];
                }
                if matches!(
                    object.get("place").and_then(Value::as_str),
                    Some("device" | "host")
                ) && find_model(model).is_none_or(|model| task_for_model(model) != Some(task))
                {
                    return vec![SettingDiagnostic::new(
                        task,
                        LocalizedMessage::new("settings.model_task_invalid")
                            .with_param("model", model)
                            .with_param("task", task),
                    )];
                }
            }
        }

        if field == "place" {
            if let Some(place) = object.get("place").and_then(Value::as_str) {
                if find_provider(place).is_none_or(|provider| {
                    !strings(provider.get("tasks")).any(|supported| supported == task)
                }) {
                    return vec![SettingDiagnostic::new(
                        task,
                        LocalizedMessage::new("settings.place_task_invalid")
                            .with_param("place", place)
                            .with_param("task", task),
                    )];
                }
            }
        }

        if field == "build"
            && object
                .get("place")
                .and_then(Value::as_str)
                .is_some_and(|place| !matches!(place, "device" | "host"))
        {
            return vec![SettingDiagnostic::new(
                task,
                LocalizedMessage::new("settings.provider_build_invalid"),
            )];
        }
    }

    vec![SettingDiagnostic::new(
        task,
        LocalizedMessage::new("settings.stage_invalid"),
    )]
}

fn numeric_value(value: &Value) -> Option<f64> {
    let value = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.parse().ok()?,
        _ => return None,
    };
    value.is_finite().then_some(value)
}

fn parse_stage(task: &str, input: &Value) -> Result<SpeechStage, StageFailure> {
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
        let Some(model) = find_model(&model_id) else {
            return Err("model".to_owned().into());
        };
        if task_for_model(model) != Some(task) {
            return Err("model".to_owned().into());
        }
        if let Some(build) = build.as_ref() {
            let listed = model
                .get("builds")
                .into_iter()
                .flat_map(values)
                .any(|candidate| field_str(candidate, "engine") == Some(build.engine.as_str()));
            if !listed {
                return Err("build.engine".to_owned().into());
            }
        }
        (Some(model), model_schema(model))
    } else {
        let Some(provider) = find_provider(&place) else {
            return Err("place".to_owned().into());
        };
        if !strings(provider.get("tasks")).any(|candidate| candidate == task) {
            return Err("place".to_owned().into());
        }
        if build.is_some() {
            return Err("build".to_owned().into());
        }
        (
            None,
            task_schema(provider, task).unwrap_or_else(|| null_value()),
        )
    };

    let options = validate_options(schema, &given, model).map_err(StageFailure::Option)?;
    Ok(SpeechStage {
        place,
        model: model_id,
        options,
        build,
    })
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

fn valid_model_id(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    value.chars().count() <= 120
        && first.is_ascii_alphanumeric()
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'))
}

fn validate_options(
    schema: &Value,
    given: &Map<String, Value>,
    model: Option<&Value>,
) -> Result<Map<String, Value>, OptionFailure> {
    let mut known = std::collections::HashMap::new();
    for option in values(schema) {
        if let Some(id) = field_str(option, "id") {
            known.insert(id, option);
        }
    }
    let mut unknown = given
        .keys()
        .filter(|key| !known.contains_key(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    unknown.sort();
    if let Some(first) = unknown.first() {
        return Err(OptionFailure::new(
            first.clone(),
            OptionFailureKind::Unknown,
        ));
    }

    let mut result = Map::new();
    for option in values(schema) {
        let Some(id) = field_str(option, "id") else {
            continue;
        };
        if let Some(value) = given.get(id) {
            result.insert(id.to_owned(), option_value(option, id, value, model)?);
        } else if let Some(default) = option.get("default") {
            result.insert(id.to_owned(), normalized_default(option, default));
        }
    }
    Ok(result)
}

// Python settings.py uses repr(value), then caps the displayed representation at 40 characters.
fn shown(value: &Value) -> String {
    let representation = python_repr(value);
    if representation.chars().count() <= 40 {
        representation
    } else {
        representation.chars().take(39).collect::<String>() + "…"
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(value) => {
            let quote = if value.contains('\'') && !value.contains('"') { '"' } else { '\'' };
            let mut result = String::from(quote);
            for character in value.chars() {
                match character {
                    '\\' => result.push_str("\\\\"),
                    '\n' => result.push_str("\\n"),
                    '\r' => result.push_str("\\r"),
                    '\t' => result.push_str("\\t"),
                    character if character == quote => {
                        result.push('\\');
                        result.push(character);
                    }
                    character if character.is_control() || (character.is_whitespace() && character != ' ') => {
                        let code = character as u32;
                        if code <= 0xff {
                            result.push_str(&format!("\\x{code:02x}"));
                        } else if code <= 0xffff {
                            result.push_str(&format!("\\u{code:04x}"));
                        } else {
                            result.push_str(&format!("\\U{code:08x}"));
                        }
                    }
                    character => result.push(character),
                }
            }
            result.push(quote);
            result
        }
        Value::Array(items) => format!("[{}]", items.iter().map(python_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(items) => format!(
            "{{{}}}",
            items.iter().map(|(key, value)| format!("{}: {}", python_repr(&Value::String(key.clone())), python_repr(value))).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn option_value(
    option: &Value,
    name: &str,
    value: &Value,
    model: Option<&Value>,
) -> Result<Value, OptionFailure> {
    let other = || OptionFailure::new(name, OptionFailureKind::Other);
    match field_str(option, "kind") {
        Some("language") => {
            if value.as_str().is_some_and(|language| {
                strings(option.get("values")).any(|candidate| candidate == language)
                    || (language == "auto" && option.get("auto").and_then(Value::as_bool) == Some(true))
            })
            {
                Ok(value.clone())
            } else {
                Err(OptionFailure::new(
                    name,
                    OptionFailureKind::Language {
                        shown: shown(value),
                    },
                ))
            }
        }
        Some("text") => {
            let max = option.get("max").and_then(Value::as_u64).unwrap_or(1000) as usize;
            if value.as_str().is_some_and(|text| text.chars().count() <= max) {
                Ok(value.clone())
            } else {
                Err(OptionFailure::new(
                    name,
                    OptionFailureKind::TextInvalid { max },
                ))
            }
        }
        Some("range") => {
            let minimum = option.get("min").cloned().unwrap_or(Value::Null);
            let maximum = option.get("max").cloned().unwrap_or(Value::Null);
            let range_failure = || {
                OptionFailure::new(
                    name,
                    OptionFailureKind::Range(Box::new((minimum.clone(), maximum.clone()))),
                )
            };
            let Some(number) = value.as_f64() else {
                return Err(range_failure());
            };
            let (Some(min), Some(max)) = (minimum.as_f64(), maximum.as_f64()) else {
                return Err(range_failure());
            };
            if !number.is_finite() || !(min..=max).contains(&number) {
                return Err(range_failure());
            }
            Number::from_f64(number)
                .map(Value::Number)
                .ok_or_else(range_failure)
        }
        Some("voice") => {
            if option.get("per_language").and_then(Value::as_bool) == Some(true) {
                let Some(voices) = value.as_object() else {
                    return Err(OptionFailure::new(name, OptionFailureKind::VoiceShape));
                };
                let mut result = Map::new();
                for (language, voice) in voices {
                    if !speech_catalogue_language(language) {
                        return Err(OptionFailure::new(
                            name,
                            OptionFailureKind::VoiceSpeechLanguage {
                                shown: shown(&Value::String(language.clone())),
                            },
                        ));
                    }
                    result.insert(
                        language.clone(),
                        Value::String(voice_id(option, name, voice, model, Some(language))?),
                    );
                }
                Ok(Value::Object(result))
            } else {
                Ok(Value::String(voice_id(option, name, value, model, None)?))
            }
        }
        _ => Err(other()),
    }
}

fn voice_id(
    option: &Value,
    name: &str,
    voice: &Value,
    model: Option<&Value>,
    language: Option<&str>,
) -> Result<String, OptionFailure> {
    if field_str(option, "from") == Some("model.voices") {
        let found = voice.as_str().and_then(|voice_id| {
            model
                .and_then(|model| model.get("voices"))
                .into_iter()
                .flat_map(values)
                .find(|candidate| field_str(candidate, "id") == Some(voice_id))
        });
        let Some(found) = found else {
            return Err(OptionFailure::new(
                name,
                OptionFailureKind::ModelVoice { shown: shown(voice) },
            ));
        };
        let voice = voice.as_str().expect("matched catalogue voice is a string");
        if let Some(language) = language {
            let speaks =
                field_str(found, "language").unwrap_or("").split('-').next() == Some(language);
            if !speaks {
                return Err(OptionFailure::new(
                    name,
                    OptionFailureKind::VoiceLanguage {
                        shown: shown(&Value::String(voice.to_owned())),
                        language: language.to_owned(),
                    },
                ));
            }
        }
        Ok(voice.to_owned())
    } else if voice.as_str().is_some_and(|voice| !voice.trim().is_empty() && voice.chars().count() <= 120) {
        Ok(voice.as_str().unwrap().to_owned())
    } else {
        Err(OptionFailure::new(name, OptionFailureKind::ProviderVoice))
    }
}

/// Whether the call settings cannot run because the host is not an available model place or a provider is
/// missing a key/voice. The caller supplies only key availability; T2 never reads storage or environment state.
pub fn unavailable(
    settings: &CallSettings,
    provider_key_available: impl Fn(&str) -> bool,
) -> Option<LocalizedMessage> {
    if settings.stt.place == "host" || settings.tts.place == "host" {
        return Some(LocalizedMessage::new("place_host_unavailable"));
    }
    for (task, stage) in [("stt", &settings.stt), ("tts", &settings.tts)] {
        if matches!(stage.place.as_str(), "device" | "host") {
            continue;
        }
        let Some(provider) = find_provider(&stage.place) else {
            continue;
        };
        if !provider_key_available(&stage.place) {
            return Some(
                LocalizedMessage::new("provider_key_missing")
                    .with_param("provider", stage.place.clone())
                    .with_param(
                        "provider_label",
                        field_str(provider, "label")
                            .unwrap_or(&stage.place)
                            .to_owned(),
                    ),
            );
        }
        if task == "tts" {
            let voice_is_missing = match stage.options.get("voice") {
                None | Some(Value::Null) => true,
                Some(Value::String(voice)) => voice.is_empty(),
                Some(Value::Object(voices)) => voices.is_empty(),
                _ => false,
            };
            if voice_is_missing {
                return Some(
                    LocalizedMessage::new("voice_missing")
                        .with_param("provider", stage.place.clone())
                        .with_param(
                            "provider_label",
                            field_str(provider, "label")
                                .unwrap_or(&stage.place)
                                .to_owned(),
                        ),
                );
            }
        }
    }
    None
}

/// A saved provider key wins; an empty/whitespace value is treated as absent.
pub fn effective_key<'a>(stored: Option<&'a str>, environment: Option<&'a str>) -> Option<&'a str> {
    stored
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| environment.map(str::trim).filter(|value| !value.is_empty()))
}

/// Return only configuration state and a last-four-character hint, never the key itself.
pub fn credential_state(stored: Option<&str>, environment: Option<&str>) -> CredentialState {
    let saved = stored.map(str::trim).filter(|value| !value.is_empty());
    let deployed = environment.map(str::trim).filter(|value| !value.is_empty());
    let (key, source) = if let Some(key) = saved {
        (Some(key), Some("stored"))
    } else if let Some(key) = deployed {
        (Some(key), Some("environment"))
    } else {
        (None, None)
    };
    let hint = key.map(|key| {
        let mut suffix = key.chars().rev().take(4).collect::<String>();
        suffix = suffix.chars().rev().collect();
        format!("…{suffix}")
    });
    CredentialState {
        configured: key.is_some(),
        source,
        hint,
    }
}

/// Resolve the voice for a reply language, keeping device and provider voice-choice behavior independent.
pub fn resolve_voice(
    settings: &CallSettings,
    language: Option<&str>,
) -> Result<ResolvedVoice, LocalizedMessage> {
    let language = language
        .filter(|language| !language.is_empty())
        .unwrap_or(&settings.ui_language);
    if !speech_catalogue_language(language) {
        return Err(LocalizedMessage::new("speech_language_unsupported")
            .with_param("language", language.to_owned()));
    }
    let stage = &settings.tts;
    if stage.place == "host" {
        return Err(LocalizedMessage::new("place_host_unavailable"));
    }
    let provider = find_provider(&stage.place);
    let schema = if matches!(stage.place.as_str(), "device" | "host") {
        let model = find_model(&stage.model)
            .ok_or_else(|| LocalizedMessage::new("speech_voice_unavailable"))?;
        model_schema(model)
    } else {
        provider
            .and_then(|provider| task_schema(provider, "tts"))
            .unwrap_or_else(|| null_value())
    };
    let voice_option = values(schema)
        .iter()
        .find(|option| field_str(option, "kind") == Some("voice"));
    let chosen = stage.options.get("voice");
    let picked = match chosen {
        Some(Value::Object(voices)) => voices.get(language),
        Some(value) => Some(value),
        None => None,
    };
    let voice =
        if voice_option.is_some_and(|option| field_str(option, "from") == Some("model.voices")) {
            let model = find_model(&stage.model)
                .ok_or_else(|| LocalizedMessage::new("speech_voice_unavailable"))?;
            let spoken = model
                .get("voices")
                .into_iter()
                .flat_map(values)
                .filter(|entry| {
                    field_str(entry, "language").unwrap_or("").split('-').next() == Some(language)
                })
                .collect::<Vec<_>>();
            let first_spoken = spoken.first().and_then(|entry| field_str(entry, "id"));
            picked
                .and_then(Value::as_str)
                .filter(|picked| {
                    spoken
                        .iter()
                        .any(|entry| field_str(entry, "id") == Some(*picked))
                })
                .or(first_spoken)
        } else {
            picked.and_then(Value::as_str).or_else(|| {
                chosen
                    .and_then(Value::as_object)
                    .and_then(|voices| voices.values().next())
                    .and_then(Value::as_str)
            })
        };
    let Some(voice) = voice.filter(|voice| !voice.is_empty()) else {
        return Err(LocalizedMessage::new("speech_voice_unavailable")
            .with_param("language", language.to_owned()));
    };
    let speed = stage
        .options
        .get("speed")
        .and_then(Value::as_f64)
        .or_else(|| {
            values(schema)
                .iter()
                .find(|option| field_str(option, "id") == Some("speed"))
                .and_then(|option| option.get("default"))
                .and_then(Value::as_f64)
        })
        .unwrap_or(1.0);
    Ok(ResolvedVoice {
        place: stage.place.clone(),
        model: stage.model.clone(),
        voice: voice.to_owned(),
        language: language.to_owned(),
        speed,
    })
}

/// Convert the device's one-word patience choice to the room's microphone detector values.
pub fn mic_settings(
    settings: &CallSettings,
    overrides: Option<&Value>,
) -> (MicSettings, Option<String>) {
    let defaults = default_settings(None, None);
    let base = MicSettings {
        turn_end_mode: defaults.turn_end_mode,
        user_speech_timeout: defaults.user_speech_timeout,
        smart_turn_min_silence: defaults.smart_turn_min_silence,
        smart_turn_max_silence: defaults.smart_turn_max_silence,
        vad_confidence: defaults.vad_confidence,
        vad_min_volume: defaults.vad_min_volume,
        vad_start_secs: defaults.vad_start_secs,
        merge_window_secs: defaults.merge_window_secs,
    };
    let patience_override = overrides
        .and_then(Value::as_object)
        .and_then(|object| object.get("turn_patience"))
        .and_then(Value::as_str);
    let patience = patience_override.unwrap_or(&settings.turn_patience);
    let mut effective = base.clone();
    match patience {
        "fast" => {
            effective.smart_turn_min_silence = 0.6;
            effective.smart_turn_max_silence = 2.5;
            effective.user_speech_timeout = 2.0;
            effective.merge_window_secs = 0.0;
            (effective, None)
        }
        "normal" => (effective, None),
        "calm" => {
            effective.smart_turn_min_silence = 1.3;
            effective.smart_turn_max_silence = 4.0;
            effective.user_speech_timeout = 3.5;
            effective.merge_window_secs = 1.5;
            (effective, None)
        }
        _ => {
            let shown = patience.chars().take(40).collect::<String>();
            (
                base,
                Some(render(
                    &LocalizedMessage::new("turn_patience_unknown").with_param("patience", shown),
                    &settings.ui_language,
                )),
            )
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CheckClip {
    pub audio: &'static [u8],
    pub text: &'static str,
}

/// The language a model check can actually exercise; unsupported/automatic languages use the English fixture.
pub fn check_language<'a>(task: &str, language: Option<&'a str>) -> &'a str {
    let collection = if task == "stt" { "clips" } else { "phrases" };
    let fallback = field_str(checks(), "fallback").unwrap_or("en");
    let supported = checks()
        .get(task)
        .and_then(|entry| entry.get(collection))
        .and_then(Value::as_object)
        .is_some_and(|entries| language.is_some_and(|language| entries.contains_key(language)));
    if supported {
        language.unwrap()
    } else {
        fallback
    }
}

/// The bundled transcription clip and its reference text for a supported language.
pub fn stt_check_clip(language: Option<&str>) -> CheckClip {
    let language = check_language("stt", language);
    let clip = checks()
        .get("stt")
        .and_then(|entry| entry.get("clips"))
        .and_then(|clips| clips.get(language))
        .expect("the selected transcription check is in the embedded spec");
    let audio = if language == "es" {
        CHECK_ES_WAV
    } else {
        CHECK_EN_WAV
    };
    CheckClip {
        audio,
        text: field_str(clip, "text").expect("check text is present"),
    }
}

/// The fixed phrase a voice model is asked to speak for a supported language.
pub fn tts_check_phrase(language: Option<&str>) -> &'static str {
    let language = check_language("tts", language);
    checks()
        .get("tts")
        .and_then(|entry| entry.get("phrases"))
        .and_then(|phrases| phrases.get(language))
        .and_then(Value::as_str)
        .expect("the selected voice check phrase is in the embedded spec")
}

/// Word error rate after Unicode compatibility decomposition, accent removal and punctuation folding.
pub fn word_error(expected: &str, heard: &str) -> f64 {
    let expected = words(expected);
    let heard = words(heard);
    if expected.is_empty() {
        return if heard.is_empty() { 0.0 } else { 1.0 };
    }
    let mut row = (0..=heard.len()).collect::<Vec<_>>();
    for (i, word) in expected.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, other) in heard.iter().enumerate() {
            let above = row[j + 1];
            let substitution = previous + usize::from(word != other);
            row[j + 1] = (above + 1).min(row[j] + 1).min(substitution);
            previous = above;
        }
    }
    row[heard.len()] as f64 / expected.len() as f64
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .map(|character| {
            if character.is_alphanumeric() || character.is_whitespace() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// A stable refusal when a transcription check is silent or too different from its fixture.
pub fn transcript_problem(expected: &str, heard: &str) -> Option<LocalizedMessage> {
    if words(heard).is_empty() {
        return Some(LocalizedMessage::new("check_silent"));
    }
    let max_error = checks()
        .get("stt")
        .and_then(|entry| entry.get("max_word_error"))
        .and_then(Value::as_f64)
        .unwrap_or(0.5);
    if word_error(expected, heard) > max_error {
        let heard = heard.trim().chars().take(200).collect::<String>();
        Some(LocalizedMessage::new("check_mismatch").with_param("heard", heard))
    } else {
        None
    }
}

/// A stable refusal for malformed, silent or implausibly short/long waveform output.
pub fn audio_problem(samples: &[f32], sample_rate: f64) -> Option<LocalizedMessage> {
    if !sample_rate.is_finite()
        || sample_rate <= 0.0
        || samples.iter().any(|sample| !sample.is_finite())
    {
        return Some(LocalizedMessage::new("check_invalid_audio"));
    }
    let count = samples.len();
    let rms = if count == 0 {
        0.0
    } else {
        (samples
            .iter()
            .map(|sample| (*sample as f64).powi(2))
            .sum::<f64>()
            / count as f64)
            .sqrt()
    };
    let minimum_rms = checks()
        .get("tts")
        .and_then(|entry| entry.get("min_rms"))
        .and_then(Value::as_f64)
        .unwrap_or(0.005);
    if rms < minimum_rms {
        return Some(LocalizedMessage::new("check_silent"));
    }
    let seconds = count as f64 / sample_rate;
    let bounds = checks()
        .get("tts")
        .and_then(|entry| entry.get("seconds"))
        .map(values)
        .unwrap_or(&[]);
    let low = bounds.first().and_then(Value::as_f64).unwrap_or(1.5);
    let high = bounds.get(1).and_then(Value::as_f64).unwrap_or(20.0);
    if !(low..=high).contains(&seconds) {
        let rounded = format!("{seconds:.2}").parse::<f64>().unwrap_or(seconds);
        let display = format!("{seconds:.1}");
        return Some(
            LocalizedMessage::new("check_duration")
                .with_param("seconds", rounded)
                .with_param("seconds_display", display),
        );
    }
    None
}

/// Latency above the comfort line is reported to the person, never treated as a failed model check.
pub fn slow(latency_ms: u64) -> bool {
    let comfort = checks()
        .get("stt")
        .and_then(|entry| entry.get("comfort_ms"))
        .and_then(Value::as_u64)
        .unwrap_or(2000);
    latency_ms > comfort
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shipped_catalogue_is_sound_and_catalogue_bytes_remain_the_source_contract() {
        assert_eq!(catalog_problems(catalog()), Vec::<String>::new());
        assert_eq!(catalog_text(), CATALOG_JSON);

        let mut broken = catalog().clone();
        broken["engines"][0]["packages"][0]["download"]["sha256"] = json!("short");
        broken["families"]["whisper"]["options"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id": "mood", "kind": "colour-wheel"
            }));
        let problems = catalog_problems(&broken);
        assert!(problems
            .iter()
            .any(|problem| problem.contains("download with sha256")));
        assert!(problems
            .iter()
            .any(|problem| problem.contains("unknown kind \"colour-wheel\"")));
    }

    #[test]
    fn shared_catalogue_offer_vectors_match_for_shipped_and_fixture_data() {
        let vectors: Value = serde_json::from_str(VECTORS_JSON).unwrap();
        let fixture = &vectors["fixture"];
        assert!(catalog_problems(fixture).is_empty());
        for vector in values(&vectors["vectors"]) {
            let name = field_str(vector, "name").unwrap_or("unnamed vector");
            let selected_catalog = match field_str(vector, "catalog") {
                Some("fixture") => fixture,
                _ => catalog(),
            };
            let place = field_str(vector, "place").unwrap_or("");
            let actual = offers(selected_catalog, &vector["capabilities"], place);
            if vector.get("error").is_some() {
                assert!(actual.is_err(), "{name}: {actual:?}");
            } else {
                let actual = actual.unwrap_or_else(|error| panic!("{name}: {error}"));
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    vector["offers"],
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn defaults_use_the_first_catalogue_model_when_capabilities_are_unknown() {
        let settings = default_settings(None, None);
        assert_eq!(
            (&*settings.stt.place, &*settings.stt.model),
            ("device", "whisper-tiny")
        );
        assert_eq!(
            (&*settings.tts.place, &*settings.tts.model),
            ("device", "kokoro-82m-v1.0")
        );
        assert_eq!(settings.stt.options["language"], json!("en"));
        assert_eq!(settings.tts.options["speed"], json!(1.0));
        assert_eq!(
            (
                settings.ui_language.as_str(),
                settings.turn_patience.as_str()
            ),
            ("en", "normal")
        );

        let wasm = json!({"runs":"page", "has":["wasm"]});
        let resolved = default_settings(None, Some(&wasm));
        assert_eq!(resolved.stt.model, "whisper-tiny");
        assert_eq!(resolved.tts.model, "kokoro-82m-v1.0");
    }

    #[test]
    fn defaults_normalize_system_language_separately_for_ui_and_speech_catalogues() {
        let spanish = default_settings(Some("es-ES"), None);
        assert_eq!(spanish.ui_language, "es");
        assert_eq!(spanish.stt.options["language"], json!("es"));

        let french = default_settings(Some("fr-FR"), None);
        assert_eq!(french.ui_language, "en");
        assert_eq!(french.stt.options["language"], json!("fr"));

        let unsupported = default_settings(Some("ja-JP"), None);
        assert_eq!(unsupported.ui_language, "en");
        assert_eq!(unsupported.stt.options["language"], json!("en"));
    }

    #[test]
    fn incoming_wire_settings_validate_ui_locale_exactly_and_fallback_at_field_boundaries() {
        let defaults = default_settings(None, None);
        let input = json!({
            "ui_language":"es-ES",
            "stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"es", "context":"Sidevoice"}},
            "tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"es":"em_alex"}, "speed":9}},
            "audio_grace_seconds":3,
            "unknown_future_key":1
        });
        let loaded = settings_from(Some(&input), &defaults);
        assert_eq!(loaded.settings.ui_language, "en");
        assert_eq!(loaded.settings.audio_grace_seconds, 3.0);
        assert_eq!(loaded.settings.stt.place, "openai");
        assert_eq!(loaded.settings.stt.options["context"], json!("Sidevoice"));
        assert_eq!(loaded.settings.tts.model, defaults.tts.model);
        assert_eq!(
            loaded.issue.as_deref(),
            Some("Some device settings were not valid and use their defaults: ui_language Input should be 'es' or 'en'; tts Value error, speed: a number from 0.5 to 2")
        );

        assert!(settings_from(None, &defaults).issue.is_none());
        assert!(settings_from(Some(&json!({"old_setting":true})), &defaults)
            .issue
            .is_none());
    }

    #[test]
    fn settings_diagnostics_match_pinned_python_for_enum_number_and_required_fields() {
        let defaults = default_settings(None, None);
        let cases = [
            (
                json!({"turn_patience":"patient"}),
                "Some device settings were not valid and use their defaults: turn_patience Input should be 'fast', 'normal' or 'calm'",
            ),
            (
                json!({"turn_end_mode":"other"}),
                "Some device settings were not valid and use their defaults: turn_end_mode Input should be 'timer' or 'smart_turn'",
            ),
            (
                json!({"audio_grace_seconds":"abc"}),
                "Some device settings were not valid and use their defaults: audio_grace_seconds Input should be a valid number, unable to parse string as a number",
            ),
            (
                json!({"tts":{}}),
                "Some device settings were not valid and use their defaults: tts.place Field required; tts.model Field required",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                settings_from(Some(&input), &defaults).issue.as_deref(),
                Some(expected),
                "Python settings.py output for {input}"
            );
        }

        let mut spanish_defaults = defaults;
        spanish_defaults.ui_language = "es".to_owned();
        assert_eq!(
            settings_from(Some(&json!({"turn_patience":"patient"})), &spanish_defaults)
                .issue
                .as_deref(),
            Some("Algunos ajustes del dispositivo no eran válidos y usan sus valores predeterminados: turn_patience El valor debe ser 'fast', 'normal' o 'calm'")
        );
    }

    #[test]
    fn inclusive_float_endpoints_match_the_python_settings_contract() {
        // Python settings.py accepts both inclusive endpoints for every numeric settings field.
        let endpoints = [
            ("audio_grace_seconds", 0.0, 10.0),
            ("replay_on_return_seconds", 0.0, 3600.0),
            ("user_speech_timeout", 0.5, 15.0),
            ("smart_turn_min_silence", 0.1, 3.0),
            ("smart_turn_max_silence", 0.5, 15.0),
            ("vad_confidence", 0.1, 1.0),
            ("vad_min_volume", 0.0, 1.0),
            ("vad_start_secs", 0.05, 1.0),
            ("merge_window_secs", 0.0, 5.0),
        ];
        let defaults = default_settings(None, None);
        for (name, minimum, maximum) in endpoints {
            for endpoint in [minimum, maximum] {
                let mut input = Map::new();
                input.insert(name.to_owned(), json!(endpoint));
                let loaded = settings_from(Some(&Value::Object(input)), &defaults);
                assert!(loaded.issue.is_none(), "{name}={endpoint}");
            }
        }

        for (name, invalid, expected) in [
            (
                "vad_confidence",
                0.09,
                "Some device settings were not valid and use their defaults: vad_confidence Input should be greater than or equal to 0.1",
            ),
            (
                "audio_grace_seconds",
                11.0,
                "Some device settings were not valid and use their defaults: audio_grace_seconds Input should be less than or equal to 10",
            ),
        ] {
            let mut input = Map::new();
            input.insert(name.to_owned(), json!(invalid));
            assert_eq!(
                settings_from(Some(&Value::Object(input)), &defaults)
                    .issue
                    .as_deref(),
                Some(expected),
                "Python settings.py diagnostic for {name}"
            );
        }
    }

    #[test]
    fn stage_rules_reject_cross_task_models_provider_builds_and_unknown_options() {
        let defaults = default_settings(None, None);
        let cases = [
            (
                json!({"stt":{"place":"device", "model":"kokoro-82m-v1.0"}}),
                "Some device settings were not valid and use their defaults: stt Value error, 'kokoro-82m-v1.0' is not a stt model of the catalogue",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "build":{"engine":"sherpa-onnx", "accelerator":"cpu"}}}),
                "Some device settings were not valid and use their defaults: stt Value error, a provider runs its own models: a build cannot be chosen",
            ),
            (
                json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"pitch":1}}}),
                "Some device settings were not valid and use their defaults: tts Value error, unknown options: 'pitch'",
            ),
            (
                json!({"stt":{"place":"openai", "model":"bad/id"}}),
                "Some device settings were not valid and use their defaults: stt Value error, 'bad/id' is not a model id",
            ),
            (
                json!({"tts":{"place":"device", "model":"missing"}}),
                "Some device settings were not valid and use their defaults: tts Value error, 'missing' is not a tts model of the catalogue",
            ),
        ];
        for (input, expected) in cases {
            let loaded = settings_from(Some(&input), &defaults);
            assert_eq!(
                loaded.issue.as_deref(),
                Some(expected),
                "Pinned Python settings.py output for {input}"
            );
        }
    }

    #[test]
    fn known_option_diagnostics_match_pinned_python_and_keep_atomic_stage_fallback() {
        let defaults = default_settings(None, None);
        let overlong_context = "x".repeat(401);
        let overlong_voice = "x".repeat(121);
        let cases = [
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"de"}}}),
                "language: 'de' is not one of its languages",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":5}}}),
                "language: 5 is not one of its languages",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":null}}}),
                "language: None is not one of its languages",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":true}}}),
                "language: True is not one of its languages",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":["de"]}}}),
                "language: ['de'] is not one of its languages",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"isn't"}}}),
                "language: \"isn't\" is not one of its languages",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"a\nb"}}}),
                "language: 'a\\nb' is not one of its languages",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"language":"x".repeat(50)}}}),
                "language: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx… is not one of its languages",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"context":overlong_context}}}),
                "context: text of at most 400 characters",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"context":7}}}),
                "context: text of at most 400 characters",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"context":["a"]}}}),
                "context: text of at most 400 characters",
                "stt",
            ),
            (
                json!({"stt":{"place":"openai", "model":"gpt-4o-transcribe", "options":{"context":null}}}),
                "context: text of at most 400 characters",
                "stt",
            ),
            (
                json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":"af_heart"}}}),
                "voice: one voice per speech language, as {language: voice}",
                "tts",
            ),
            (
                json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"de":"af_heart"}}}}),
                "voice: 'de' is not a speech language",
                "tts",
            ),
            (
                json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"en":"no-such-voice"}}}}),
                "voice: 'no-such-voice' is not a voice of this model",
                "tts",
            ),
            (
                json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"en":5}}}}),
                "voice: 5 is not a voice of this model",
                "tts",
            ),
            (
                json!({"tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"en":"em_alex"}}}}),
                "voice: 'em_alex' does not speak en",
                "tts",
            ),
            (
                json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":"v"}}}),
                "voice: one voice per speech language, as {language: voice}",
                "tts",
            ),
            (
                json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"xx":"v"}}}}),
                "voice: 'xx' is not a speech language",
                "tts",
            ),
            (
                json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"en":""}}}}),
                "voice: a voice id is a non-empty string of at most 120 characters",
                "tts",
            ),
            (
                json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"en":"   "}}}}),
                "voice: a voice id is a non-empty string of at most 120 characters",
                "tts",
            ),
            (
                json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"en":overlong_voice}}}}),
                "voice: a voice id is a non-empty string of at most 120 characters",
                "tts",
            ),
            (
                json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{"en":null}}}}),
                "voice: a voice id is a non-empty string of at most 120 characters",
                "tts",
            ),
        ];
        for (input, reason, task) in cases {
            let expected = format!(
                "Some device settings were not valid and use their defaults: {task} Value error, {reason}"
            );
            let loaded = settings_from(Some(&input), &defaults);
            assert_eq!(
                loaded.issue.as_deref(),
                Some(expected.as_str()),
                "Pinned Python settings.py output for {task} option in {input}"
            );
            if task == "stt" {
                assert_eq!(loaded.settings.stt.place, defaults.stt.place);
                assert_eq!(loaded.settings.stt.model, defaults.stt.model);
                assert_eq!(loaded.settings.stt.options, defaults.stt.options);
            } else {
                assert_eq!(loaded.settings.tts.place, defaults.tts.place);
                assert_eq!(loaded.settings.tts.model, defaults.tts.model);
                assert_eq!(loaded.settings.tts.options, defaults.tts.options);
            }
        }
    }

    #[test]
    fn shown_matches_python_repr_at_the_display_limit() {
        assert_eq!(super::shown(&json!("x".repeat(38))), format!("'{}'", "x".repeat(38)));
        assert_eq!(super::shown(&json!("x".repeat(39))), format!("'{}…", "x".repeat(38)));
        assert_eq!(super::shown(&json!("a'b\"c")), "'a\\'b\"c'");
        assert_eq!(super::shown(&json!("a\\b")), "'a\\\\b'");
    }

    #[test]
    fn invalid_stage_is_atomic_while_other_fields_and_stage_survive() {
        let defaults = default_settings(None, None);
        let input = json!({
            "stt":{"place":"openai", "model":"gpt-4o-transcribe"},
            "tts":{"place":"device", "model":"kokoro-82m-v1.0", "options":{"voice":{"es":"em_alex"}, "speed":3}},
            "vad_confidence":2
        });
        let loaded = settings_from(Some(&input), &defaults);
        assert_eq!(loaded.settings.stt.place, "openai");
        assert_eq!(loaded.settings.tts.place, defaults.tts.place);
        assert_eq!(loaded.settings.tts.model, defaults.tts.model);
        assert_eq!(loaded.settings.tts.options, defaults.tts.options);
        assert_eq!(loaded.settings.vad_confidence, defaults.vad_confidence);
        assert_eq!(
            loaded.issue.as_deref(),
            Some("Some device settings were not valid and use their defaults: tts Value error, speed: a number from 0.5 to 2; vad_confidence Input should be less than or equal to 1")
        );
    }

    #[test]
    fn key_precedence_and_hints_never_return_a_secret() {
        assert_eq!(
            effective_key(Some("  saved-secret "), Some("environment-secret")),
            Some("saved-secret")
        );
        assert_eq!(
            effective_key(Some("  "), Some(" env-secret ")),
            Some("env-secret")
        );
        assert_eq!(effective_key(None, Some(" \n ")), None);
        let stored = credential_state(Some(" secret-1234 "), Some("environment"));
        assert_eq!(
            stored,
            CredentialState {
                configured: true,
                source: Some("stored"),
                hint: Some("…1234".to_owned())
            }
        );
        let environment = credential_state(None, Some("env-5678"));
        assert_eq!(environment.source, Some("environment"));
        assert_eq!(environment.hint.as_deref(), Some("…5678"));
        assert_eq!(credential_state(None, None).hint, None);
    }

    #[test]
    fn availability_refusals_keep_host_precedence_then_provider_key_then_voice() {
        let defaults = default_settings(None, None);
        let host_input = json!({"tts":{"place":"host", "model":"kokoro-82m-v1.0"}});
        let host = settings_from(Some(&host_input), &defaults).settings;
        assert_eq!(
            unavailable(&host, |_| false).unwrap().key,
            "place_host_unavailable"
        );

        let provider_input =
            json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{"voice":{}}}});
        let provider = settings_from(Some(&provider_input), &defaults).settings;
        assert_eq!(
            unavailable(&provider, |_| false).unwrap().key,
            "provider_key_missing"
        );
        let missing_voice = unavailable(&provider, |_| true).unwrap();
        assert_eq!(missing_voice.key, "voice_missing");
        assert_eq!(missing_voice.params["provider"], json!("elevenlabs"));
        assert!(unavailable(&defaults, |_| true).is_none());
    }

    #[test]
    fn reply_voice_resolution_uses_model_language_then_provider_selection_order() {
        let defaults = default_settings(None, None);
        let spanish_voice = resolve_voice(&defaults, Some("es")).unwrap();
        assert_eq!(spanish_voice.voice, "ef_dora");
        assert_eq!(
            resolve_voice(&defaults, Some("en")).unwrap().voice,
            "af_heart"
        );

        let input = json!({"tts":{"place":"elevenlabs", "model":"eleven_v3", "options":{
            "voice":{"es":"voz-espanola", "en":"english-voice"}, "speed":1.1
        }}});
        let settings = settings_from(Some(&input), &defaults).settings;
        assert_eq!(
            resolve_voice(&settings, Some("en")).unwrap().voice,
            "english-voice"
        );
        let fallback = resolve_voice(&settings, Some("fr")).unwrap();
        assert_eq!(fallback.voice, "voz-espanola");
        assert_eq!(fallback.speed, 1.1);
        assert_eq!(resolve_voice(&settings, None).unwrap().language, "en");
        assert_eq!(
            resolve_voice(&settings, Some("ja")).unwrap_err().key,
            "speech_language_unsupported"
        );
    }

    #[test]
    fn patience_maps_only_the_existing_room_presets() {
        let defaults = default_settings(None, None);
        let mut fast = defaults.clone();
        fast.turn_patience = "fast".to_owned();
        let (fast, problem) = mic_settings(&fast, None);
        assert!(problem.is_none());
        assert_eq!(
            (
                fast.smart_turn_min_silence,
                fast.smart_turn_max_silence,
                fast.user_speech_timeout,
                fast.merge_window_secs
            ),
            (0.6, 2.5, 2.0, 0.0)
        );

        let (calm, problem) = mic_settings(&defaults, Some(&json!({"turn_patience":"calm"})));
        assert!(problem.is_none());
        assert_eq!(
            (
                calm.smart_turn_min_silence,
                calm.smart_turn_max_silence,
                calm.user_speech_timeout,
                calm.merge_window_secs
            ),
            (1.3, 4.0, 3.5, 1.5)
        );

        let (fallback, problem) =
            mic_settings(&defaults, Some(&json!({"turn_patience":"patient"})));
        assert_eq!(fallback.merge_window_secs, defaults.merge_window_secs);
        assert_eq!(
            problem.as_deref(),
            Some("Unknown patience; the room's own is used: patient")
        );
    }

    #[test]
    fn model_check_fixtures_and_transcript_verdicts_follow_existing_rules() {
        let spanish = stt_check_clip(Some("es"));
        assert_eq!(spanish.text, tts_check_phrase(Some("es")));
        assert!(!spanish.audio.is_empty());
        assert_eq!(check_language("stt", Some("auto")), "en");
        assert_eq!(check_language("tts", Some("fr")), "en");
        assert_eq!(word_error(spanish.text, &spanish.text.to_uppercase()), 0.0);
        let unaccented = " hola esto es una prueba de transcripcion para comprobar que el modelo entiende lo que digo";
        assert!(transcript_problem(spanish.text, unaccented).is_none());
        assert_eq!(
            transcript_problem(spanish.text, "... ").unwrap().key,
            "check_silent"
        );
        let mismatch = transcript_problem(spanish.text, "Thank you for watching.").unwrap();
        assert_eq!(mismatch.key, "check_mismatch");
        assert_eq!(mismatch.params["heard"], json!("Thank you for watching."));
    }

    #[test]
    fn model_check_audio_verdicts_and_latency_cutoff_match_the_embedded_spec() {
        let tone = (0..80_000)
            .map(|index| (0.3 * (index as f64 / 10.0).sin()) as f32)
            .collect::<Vec<_>>();
        assert!(audio_problem(&tone, 16_000.0).is_none());
        assert_eq!(
            audio_problem(&vec![0.0; 80_000], 16_000.0).unwrap().key,
            "check_silent"
        );
        assert_eq!(
            audio_problem(&tone[..1_600], 16_000.0).unwrap().key,
            "check_duration"
        );
        let duration = audio_problem(&tone[..1_920], 16_000.0).unwrap();
        assert_eq!(duration.params["seconds"], json!(0.12));
        assert_eq!(duration.params["seconds_display"], json!("0.1"));
        assert_eq!(
            Value::Object(crate::messages::render_refusal(&duration, "en")),
            json!({
                "key":"check_duration",
                "seconds":0.12,
                "message":"The model produced 0.1 s of audio for a phrase that takes about five."
            })
        );
        assert_eq!(
            audio_problem(&tone, f64::INFINITY).unwrap().key,
            "check_invalid_audio"
        );
        let mut not_finite = tone.clone();
        not_finite[777] = f32::NAN;
        assert_eq!(
            audio_problem(&not_finite, 16_000.0).unwrap().key,
            "check_invalid_audio"
        );

        let comfort = checks()["stt"]["comfort_ms"].as_u64().unwrap();
        assert!(!slow(comfort));
        assert!(slow(comfort + 1));
    }
}
