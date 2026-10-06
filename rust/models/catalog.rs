//! The embedded model and speech-language catalogues, and lookups into them.

use std::sync::OnceLock;

use serde_json::Value;

use super::json::{field_str, values};

pub(super) const CATALOG_JSON: &str = include_str!("../../src/sidevoice_core/models/catalog.json");
const VOICE_CATALOG_JSON: &str = include_str!("../../src/sidevoice_core/pipeline/catalog.json");

static CATALOG: OnceLock<Value> = OnceLock::new();
static VOICE_CATALOG: OnceLock<Value> = OnceLock::new();
static NULL: Value = Value::Null;

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

pub fn voice_languages() -> &'static Value {
    &voice_catalog()["languages"]
}

/// Whether `language` is one of the speech languages, which are wider than the UI locales.
pub(super) fn speech_catalogue_language(language: &str) -> bool {
    voice_catalog()
        .get("languages")
        .into_iter()
        .flat_map(values)
        .any(|entry| field_str(entry, "id") == Some(language))
}

pub(super) fn find_model(model_id: &str) -> Option<&'static Value> {
    catalog()
        .get("models")
        .into_iter()
        .flat_map(values)
        .find(|model| field_str(model, "id") == Some(model_id))
}

pub(super) fn find_provider(provider_id: &str) -> Option<&'static Value> {
    catalog()
        .get("providers")
        .into_iter()
        .flat_map(values)
        .find(|provider| field_str(provider, "id") == Some(provider_id))
}

pub(super) fn task_for_model(model: &Value) -> Option<&str> {
    field_str(model, "family")
        .and_then(|family| catalog().get("families")?.get(family))
        .and_then(|family| field_str(family, "task"))
}

/// The option schema of a model's family, or JSON null when it has none.
pub(super) fn model_schema(model: &Value) -> &'static Value {
    let family = field_str(model, "family").unwrap_or("");
    catalog()
        .get("families")
        .and_then(|families| families.get(family))
        .and_then(|family| family.get("options"))
        .unwrap_or(&NULL)
}

/// The option schema a provider declares for one task, or JSON null when it has none.
pub(super) fn provider_schema(provider_id: &str, task: &str) -> &'static Value {
    find_provider(provider_id)
        .and_then(|provider| provider.get(task))
        .and_then(|entry| entry.get("options"))
        .unwrap_or(&NULL)
}
