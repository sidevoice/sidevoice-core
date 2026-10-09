use super::template::interpolate;
use super::{bundle, render, ui_locale, LocalizedMessage};
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
    let message = LocalizedMessage::new("room.conversation_title").with_param("id", json!("7"));
    assert_eq!(render(&message, "es-MX"), "Conversación 7");

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
fn english_templates_keep_their_wording() {
    let expected = [
        ("room.conversation_title", "Conversation 7"),
        (
            "runtime.log_failure",
            "The core could not start (bind.port-in-use).",
        ),
        (
            "settings.ui_language_invalid",
            "Input should be 'es' or 'en'",
        ),
        ("room.receipt_invalid", "Invalid playback state."),
        ("room.text_too_long", "The message is too long."),
    ];
    let messages = [
        LocalizedMessage::new("room.conversation_title").with_param("id", json!("7")),
        LocalizedMessage::new("runtime.log_failure").with_param("key", json!("bind.port-in-use")),
        LocalizedMessage::new("settings.ui_language_invalid"),
        LocalizedMessage::new("room.receipt_invalid"),
        LocalizedMessage::new("room.text_too_long"),
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
        "connector.credential_refused",
        "connector.protocol_unsupported",
    ] {
        assert_ne!(render(&LocalizedMessage::new(key), "en"), key, "{key}");
    }
}
