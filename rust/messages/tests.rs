use super::template::interpolate;
use super::{bundle, render, render_refusal, ui_locale, LocalizedMessage};
use serde_json::{json, Map, Value};

#[test]
fn locale_uses_the_supported_primary_subtag_and_english_fallback() {
    assert_eq!(ui_locale("es-ES"), "es");
    assert_eq!(ui_locale("es_MX"), "es");
    assert_eq!(ui_locale("fr-FR"), "en");
    assert_eq!(ui_locale(""), "en");
}

#[test]
fn render_substitutes_named_parameters_and_falls_back_to_english() {
    let message =
        LocalizedMessage::new("provider_key_missing").with_param("provider_label", json!("OpenAI"));
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
        (
            "speech_language_unsupported",
            "Unsupported speech language.",
        ),
        ("check_invalid", "The invalid request details."),
        (
            "check_rate_limited",
            "Too many model checks; try again in 4 s.",
        ),
        (
            "turn_patience_unknown",
            "Unknown patience; the room's own is used: patient",
        ),
        ("provider_key_refused", "OpenAI refused the key."),
        ("provider_unreachable", "OpenAI could not be reached."),
        ("provider_failed", "OpenAI failed: timeout."),
    ];
    let messages = [
        LocalizedMessage::new("speech_language_unsupported"),
        LocalizedMessage::new("check_invalid")
            .with_param("details", json!("The invalid request details.")),
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

#[test]
fn templates_substitute_formats_and_join_parameters() {
    let params: Map<String, Value> = json!({
        "seconds": 0.123, "label": "OpenAI", "items": ["a", "b"], "empty": null, "count": 3
    })
    .as_object()
    .unwrap()
    .clone();
    assert_eq!(
        interpolate("{seconds:.1f} s by {label}", &params),
        "0.1 s by OpenAI"
    );
    assert_eq!(interpolate("{label:.1f}", &params), "OpenAI");
    assert_eq!(
        interpolate("[{items}] [{empty}] {count}", &params),
        "[a, b] [] 3"
    );
    assert_eq!(interpolate("a{missing}b", &params), "ab");
    assert_eq!(interpolate("open {label", &params), "open {label");
}

fn placeholders(template: &str) -> Vec<&str> {
    let mut found: Vec<&str> = template
        .split('{')
        .skip(1)
        .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
        .collect();
    found.sort_unstable();
    found
}

#[test]
fn spanish_says_every_english_key_with_the_same_placeholders() {
    let english = bundle("en").as_object().unwrap();
    let spanish = bundle("es").as_object().unwrap();
    for (key, template) in english {
        let local = spanish.get(key).and_then(Value::as_str);
        assert!(local.is_some(), "es.json lacks {key}");
        assert_eq!(
            placeholders(local.unwrap()),
            placeholders(template.as_str().unwrap()),
            "{key}"
        );
    }
    assert!(
        spanish.keys().all(|key| english.contains_key(key)),
        "es.json has keys en.json does not"
    );
}

#[test]
fn keys_the_server_emits_have_english_text() {
    for key in [
        "connector-error",
        "connector-unavailable",
        "origin-not-allowed",
        "integration_superseded",
        "connector.credential_refused",
        "connector.protocol_unsupported",
    ] {
        assert_ne!(render(&LocalizedMessage::new(key), "en"), key, "{key}");
    }
}
