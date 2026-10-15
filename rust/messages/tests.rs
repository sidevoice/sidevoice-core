use super::template::interpolate;
use super::{bundle, offered, render, ui_locale, LocalizedMessage, BUNDLES, FALLBACK};
use serde_json::{json, Map, Value};

#[test]
fn english_is_the_only_bundle_and_every_language_tag_falls_back_to_it() {
    assert_eq!(FALLBACK, "en");
    assert_eq!(BUNDLES.map(|(locale, _)| locale), ["en"]);
    for tag in [
        "en",
        "en-GB",
        "EN_us",
        "en_US.UTF-8",
        "es-ES",
        "es_MX",
        "fr-FR",
        "",
    ] {
        assert_eq!(ui_locale(tag), "en", "{tag}");
    }
}

#[test]
fn a_page_names_a_registered_language_exactly() {
    assert_eq!(offered("en"), Some("en"));
    for language in ["es", "fr", "EN", "en-US", ""] {
        assert_eq!(offered(language), None, "{language}");
    }
}

#[test]
fn render_substitutes_named_parameters_and_falls_back_to_english() {
    let message = LocalizedMessage::new("room.conversation_title").with_param("id", json!("7"));
    assert_eq!(render(&message, "es-MX"), "Conversation 7");

    let fallback = LocalizedMessage::new("device.unpaired");
    assert_eq!(
        render(&fallback, "fr"),
        "This device is not paired with the core."
    );

    let unknown = LocalizedMessage::new("not.a.key");
    assert_eq!(render(&unknown, "en"), "not.a.key");
}

#[test]
fn t1_runtime_keys_are_present_and_render_without_parameters() {
    let keys = [
        "device.unpaired",
        "device.secret_invalid",
        "device.nonce_invalid",
        "device.not_found",
        "device.name_invalid",
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
            "The interface language is not valid.",
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

/// A bundle registered later must say every English key, with the same placeholders, and nothing else.
#[test]
fn every_bundle_says_every_english_key_with_the_same_placeholders() {
    let english = bundle(FALLBACK).as_object().unwrap();
    for (locale, _) in BUNDLES {
        let local = bundle(locale).as_object().unwrap();
        for (key, template) in english {
            let text = local.get(key).and_then(Value::as_str);
            assert!(text.is_some(), "{locale} lacks {key}");
            assert_eq!(
                placeholders(text.unwrap()),
                placeholders(template.as_str().unwrap()),
                "{locale} {key}"
            );
        }
        assert!(
            local.keys().all(|key| english.contains_key(key)),
            "{locale} has keys en.json does not"
        );
    }
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

#[test]
fn no_bundle_tells_the_person_about_a_room() {
    for (locale, _) in BUNDLES {
        for (key, template) in bundle(locale).as_object().unwrap() {
            let text = template.as_str().unwrap().replace("--room-credential", "");
            let says_room = text
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| matches!(word.to_lowercase().as_str(), "room" | "rooms"));
            assert!(!says_room, "{locale} {key}: {text}");
        }
    }
}
