use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

static ENGLISH: OnceLock<Value> = OnceLock::new();
static SPANISH: OnceLock<Value> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocalizedMessage {
    pub key: String,
    #[serde(default)]
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

    interpolate(template, &message.params)
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
    slot.get_or_init(|| serde_json::from_str(source).expect("embedded message bundle is valid JSON"))
}

fn interpolate(template: &str, params: &Map<String, Value>) -> String {
    let mut output = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(open) = remaining.find('{') {
        output.push_str(&remaining[..open]);
        let after_open = &remaining[open + 1..];
        let Some(close) = after_open.find('}') else {
            output.push_str(&remaining[open..]);
            return output;
        };
        let name = &after_open[..close];
        if let Some(value) = params.get(name) {
            output.push_str(&parameter(value));
        }
        remaining = &after_open[close + 1..];
    }
    output.push_str(remaining);
    output
}

fn parameter(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Array(values) => values.iter().map(parameter).collect::<Vec<_>>().join(", "),
        value => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{render, ui_locale, LocalizedMessage};
    use serde_json::json;

    #[test]
    fn locale_uses_the_supported_primary_subtag_and_english_fallback() {
        assert_eq!(ui_locale("es-ES"), "es");
        assert_eq!(ui_locale("es_MX"), "es");
        assert_eq!(ui_locale("fr-FR"), "en");
        assert_eq!(ui_locale(""), "en");
    }

    #[test]
    fn render_substitutes_named_parameters_and_falls_back_to_english() {
        let message = LocalizedMessage::new("settings.invalid")
            .with_param("fields", json!("stt.options.voice"));
        assert_eq!(
            render(&message, "es-MX"),
            "Algunos ajustes del dispositivo no son válidos y usan sus valores predeterminados: stt.options.voice."
        );

        let fallback = LocalizedMessage::new("device.unpaired");
        assert_eq!(render(&fallback, "fr"), "This device is not paired with the core.");
    }

    #[test]
    fn t1_runtime_keys_are_present_and_render_without_parameters() {
        let keys = [
            "device.unpaired",
            "device.secret_invalid",
            "device.nonce_invalid",
            "device.not_found",
            "request.origin_invalid",
            "request.host_invalid",
            "request.not_found",
            "identity.unsafe-directory",
            "identity.unreadable",
            "bind.core-running",
            "bind.port-in-use",
            "start.failed",
        ];
        for key in keys {
            let rendered = render(&LocalizedMessage::new(key), "en");
            assert!(!rendered.is_empty(), "{key}");
            assert_ne!(rendered, key, "{key}");
        }
    }
}
