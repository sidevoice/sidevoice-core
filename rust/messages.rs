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

    interpolate(template, &message.params)
}

/// Render the existing flat refusal shape, keeping message parameters at the top level.
///
/// Only fields present in the Python refusal contract are copied. Internal rendering parameters such as
/// `provider_label` and `seconds_display` are never exposed on the wire.
pub fn render_refusal(message: &LocalizedMessage, language: &str) -> Map<String, Value> {
    let mut refusal = Map::new();
    refusal.insert("key".to_owned(), Value::String(message.key.clone()));
    let wire_fields: &[&str] = match message.key.as_str() {
        "provider_key_missing" | "voice_missing" | "provider_key_refused" | "provider_unreachable" => {
            &["provider"]
        }
        "provider_failed" => &["provider", "detail"],
        "check_mismatch" => &["heard"],
        "check_duration" => &["seconds"],
        "check_rate_limited" => &["retry_after", "scope"],
        _ => &[],
    };
    for field in wire_fields {
        if let Some(value) = message.params.get(*field) {
            refusal.insert((*field).to_owned(), value.clone());
        }
    }
    refusal.insert("message".to_owned(), Value::String(render(message, language)));
    refusal
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
        let expression = &after_open[..close];
        let (name, format_spec) = expression
            .split_once(':')
            .map_or((expression, None), |(name, spec)| (name, Some(spec)));
        if let Some(value) = params.get(name) {
            if format_spec == Some(".1f") {
                if let Some(number) = value.as_f64() {
                    output.push_str(&format!("{number:.1}"));
                } else {
                    output.push_str(&parameter(value));
                }
            } else {
                output.push_str(&parameter(value));
            }
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
    use super::{render, render_refusal, ui_locale, LocalizedMessage};
    use serde_json::{json, Value};

    #[test]
    fn locale_uses_the_supported_primary_subtag_and_english_fallback() {
        assert_eq!(ui_locale("es-ES"), "es");
        assert_eq!(ui_locale("es_MX"), "es");
        assert_eq!(ui_locale("fr-FR"), "en");
        assert_eq!(ui_locale(""), "en");
    }

    #[test]
    fn render_substitutes_named_parameters_and_falls_back_to_english() {
        let message = LocalizedMessage::new("provider_key_missing")
            .with_param("provider_label", json!("OpenAI"));
        assert_eq!(
            render(&message, "es-MX"),
            "OpenAI necesita una clave de API antes de conectarse."
        );

        let fallback = LocalizedMessage::new("device.unpaired");
        assert_eq!(
            render(&fallback, "fr"),
            "This device is not paired with the core."
        );
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

    #[test]
    fn refusal_conversion_matches_flat_python_wire_shapes_and_omits_render_only_params() {
        let missing_key = LocalizedMessage::new("provider_key_missing")
            .with_param("provider", json!("openai"))
            .with_param("provider_label", json!("OpenAI"));
        assert_eq!(
            Value::Object(render_refusal(&missing_key, "en")),
            json!({
                "key":"provider_key_missing",
                "provider":"openai",
                "message":"OpenAI needs an API key before connecting."
            })
        );

        let mismatch = LocalizedMessage::new("check_mismatch")
            .with_param("heard", json!("Thank you for watching."));
        assert_eq!(
            Value::Object(render_refusal(&mismatch, "en")),
            json!({
                "key":"check_mismatch",
                "heard":"Thank you for watching.",
                "message":"The model heard something else: \"Thank you for watching.\"."
            })
        );

        let duration = LocalizedMessage::new("check_duration")
            .with_param("seconds", json!(0.12))
            .with_param("seconds_display", json!("0.1"));
        assert_eq!(
            Value::Object(render_refusal(&duration, "en")),
            json!({
                "key":"check_duration",
                "seconds":0.12,
                "message":"The model produced 0.1 s of audio for a phrase that takes about five."
            })
        );
    }

    #[test]
    fn english_templates_retain_the_pinned_python_messages() {
        let expected = [
            ("speech_language_unsupported", "Unsupported speech language."),
            ("check_invalid", "The invalid request details."),
            (
                "check_rate_limited",
                "Too many model checks; try again in 4 s.",
            ),
            (
                "turn_patience_unknown",
                "Unknown patience; the room's own is used: patient",
            ),
            (
                "provider_key_refused",
                "OpenAI refused the key.",
            ),
            (
                "provider_unreachable",
                "OpenAI could not be reached.",
            ),
            (
                "provider_failed",
                "OpenAI failed: timeout.",
            ),
        ];
        let messages = [
            LocalizedMessage::new("speech_language_unsupported"),
            LocalizedMessage::new("check_invalid").with_param("details", json!("The invalid request details.")),
            LocalizedMessage::new("check_rate_limited").with_param("retry_after", json!(4)),
            LocalizedMessage::new("turn_patience_unknown").with_param("patience", json!("patient")),
            LocalizedMessage::new("provider_key_refused").with_param("provider_label", json!("OpenAI")),
            LocalizedMessage::new("provider_unreachable").with_param("provider_label", json!("OpenAI")),
            LocalizedMessage::new("provider_failed")
                .with_param("provider_label", json!("OpenAI"))
                .with_param("detail", json!("timeout")),
        ];
        for ((key, expected), message) in expected.into_iter().zip(messages) {
            assert_eq!(message.key, key);
            assert_eq!(render(&message, "en"), expected, "{key}");
        }
    }
}
