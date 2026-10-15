//! Stable message keys rendered from per-language bundles, with English as the fallback.
//!
//! The core speaks English only. Another language is one more bundle file in `messages/` and its row in `BUNDLES`:
//! the locale lookup, a page's choice of language and the fallback all follow the table.

mod template;

#[cfg(test)]
mod tests;

use std::sync::OnceLock;

use serde_json::{Map, Value};

/// Every registered bundle: its locale (a lowercase primary language subtag) and its embedded source. The first one
/// is the fallback.
const BUNDLES: [(&str, &str); 1] = [("en", include_str!("messages/en.json"))];

/// The fallback locale, English: a key another bundle lacks, or a language no bundle speaks, is said in it.
const FALLBACK: &str = BUNDLES[0].0;

static PARSED: OnceLock<Vec<Value>> = OnceLock::new();

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

/// The registered locale for a system or device language tag, with English as the fallback.
pub fn ui_locale(language: &str) -> &'static str {
    let primary = primary_subtag(language).to_ascii_lowercase();
    offered(&primary).unwrap_or(FALLBACK)
}

/// The registered locale a page names exactly, as it sends it; none for a language no bundle speaks.
pub fn offered(language: &str) -> Option<&'static str> {
    BUNDLES
        .iter()
        .map(|(locale, _)| *locale)
        .find(|locale| *locale == language)
}

/// Render a stable message key using its bundle template and named parameters.
/// Missing locale keys fall back to English; unknown keys remain visible as their stable key.
pub fn render(message: &LocalizedMessage, language: &str) -> String {
    let local = bundle(ui_locale(language));
    let english = bundle(FALLBACK);
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

/// The parsed bundle of a registered locale; the fallback's for any other.
fn bundle(locale: &str) -> &'static Value {
    let parsed = PARSED.get_or_init(|| {
        BUNDLES
            .iter()
            .map(|(_, source)| {
                serde_json::from_str(source).expect("embedded message bundle is valid JSON")
            })
            .collect()
    });
    let index = BUNDLES
        .iter()
        .position(|(registered, _)| *registered == locale)
        .unwrap_or(0);
    &parsed[index]
}
