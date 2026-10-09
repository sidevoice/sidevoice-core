//! The embedded remote-provider and speech-language catalogues, and lookups into them.

use std::sync::OnceLock;

use serde_json::Value;

use super::json::{field_str, values};

const PROVIDERS_JSON: &str = include_str!("../../assets/catalog/providers.json");
const VOICE_CATALOG_JSON: &str = include_str!("../../assets/catalog/pipeline/catalog.json");

static PROVIDERS: OnceLock<Value> = OnceLock::new();
static VOICE_CATALOG: OnceLock<Value> = OnceLock::new();
static NULL: Value = Value::Null;

fn providers() -> &'static Value {
    PROVIDERS.get_or_init(|| {
        serde_json::from_str(PROVIDERS_JSON).expect("embedded provider catalogue is valid JSON")
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

/// The speech languages, in catalogue order; wider than the UI locales.
pub(super) fn speech_languages() -> impl Iterator<Item = &'static str> {
    voice_catalog()
        .get("languages")
        .into_iter()
        .flat_map(values)
        .filter_map(|entry| field_str(entry, "id"))
}

/// Whether `language` is one of the speech languages.
pub(super) fn speech_catalogue_language(language: &str) -> bool {
    speech_languages().any(|candidate| candidate == language)
}

pub(super) fn find_provider(provider_id: &str) -> Option<&'static Value> {
    providers()
        .get("providers")
        .into_iter()
        .flat_map(values)
        .find(|provider| field_str(provider, "id") == Some(provider_id))
}

/// The option schema a provider declares for one task, or JSON null when it has none.
pub(super) fn provider_schema(provider_id: &str, task: &str) -> &'static Value {
    find_provider(provider_id)
        .and_then(|provider| provider.get(task))
        .and_then(|entry| entry.get("options"))
        .unwrap_or(&NULL)
}
