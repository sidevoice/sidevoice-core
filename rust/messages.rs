//! Stable message keys rendered from per-language bundles, with English as the fallback.

mod template;

#[cfg(test)]
mod tests;

use std::sync::OnceLock;

use serde_json::{Map, Value};

/// The interface languages the core offers, as a page names them, each with its bundle. English comes first: it is
/// the fallback.
const BUNDLES: [(&str, &str); 8] = [
    ("en", include_str!("messages/en.json")),
    ("es", include_str!("messages/es.json")),
    ("fr", include_str!("messages/fr.json")),
    ("it", include_str!("messages/it.json")),
    ("de", include_str!("messages/de.json")),
    ("ja", include_str!("messages/ja.json")),
    ("zh-Hans", include_str!("messages/zh-Hans.json")),
    ("pl", include_str!("messages/pl.json")),
];

static PARSED: [OnceLock<Value>; BUNDLES.len()] = [const { OnceLock::new() }; BUNDLES.len()];

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

/// The interface language a page asked for by its exact name, if the core offers it.
pub fn offered(language: &str) -> Option<&'static str> {
    BUNDLES
        .iter()
        .map(|(locale, _)| *locale)
        .find(|locale| *locale == language)
}

/// The supported UI locale from a system or device language tag, with English as the fallback. Chinese is offered
/// only in Simplified script: a tag naming Traditional script or a region that writes it falls back to English.
pub fn ui_locale(language: &str) -> &'static str {
    let tag = language
        .split([',', ';', '.', '@'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut subtags = tag.split(['-', '_']);
    match subtags.next().unwrap_or("") {
        "zh" => {
            let traditional = subtags.any(|subtag| matches!(subtag, "hant" | "tw" | "hk" | "mo"));
            if traditional {
                "en"
            } else {
                "zh-Hans"
            }
        }
        primary => offered(primary).unwrap_or("en"),
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

fn bundle(locale: &str) -> &'static Value {
    let index = BUNDLES
        .iter()
        .position(|(name, _)| *name == locale)
        .unwrap_or(0);
    PARSED[index].get_or_init(|| {
        serde_json::from_str(BUNDLES[index].1).expect("embedded message bundle is valid JSON")
    })
}
