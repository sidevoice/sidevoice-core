//! Stable message keys rendered from per-language bundles, with English as the fallback.

mod template;

#[cfg(test)]
mod tests;

use std::sync::OnceLock;

use serde_json::{Map, Value};

static ENGLISH: OnceLock<Value> = OnceLock::new();
static SPANISH: OnceLock<Value> = OnceLock::new();

/// An internal message key and its render parameters. Convert it at a boundary; never serialize it directly.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalizedMessage {
    pub key: String,
    pub params: Map<String, Value>,
}

impl LocalizedMessage {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            params: Map::new(),
        }
    }

    pub fn with_param(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.params.insert(name.into(), value.into());
        self
    }
}

/// The supported UI locale from a system or device language tag, with English as the fallback.
pub fn ui_locale(language: &str) -> &'static str {
    match primary_subtag(language).to_ascii_lowercase().as_str() {
        "es" => "es",
        "en" => "en",
        _ => "en",
    }
}

/// Render a stable message key using its bundle template and named parameters.
/// Missing locale keys fall back to English; unknown keys remain visible as their stable key.
pub fn render(message: &LocalizedMessage, language: &str) -> String {
    let locale = ui_locale(language);
    let local = bundle(locale);
    let english = bundle("en");
    let template = local
        .get(&message.key)
        .and_then(Value::as_str)
        .or_else(|| english.get(&message.key).and_then(Value::as_str))
        .unwrap_or(&message.key);

    template::interpolate(template, &message.params)
}

fn primary_subtag(language: &str) -> &str {
    language.split(['-', '_']).next().unwrap_or("")
}

fn bundle(locale: &str) -> &'static Value {
    let (slot, source) = if locale == "es" {
        (&SPANISH, include_str!("messages/es.json"))
    } else {
        (&ENGLISH, include_str!("messages/en.json"))
    };
    slot.get_or_init(|| {
        serde_json::from_str(source).expect("embedded message bundle is valid JSON")
    })
}
