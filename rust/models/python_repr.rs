//! Python's `repr()` of a JSON value, as the pinned Python settings diagnostics display it.

use serde_json::Value;
use unicode_normalization::char::is_combining_mark;

// Python settings.py uses repr(value), then caps the displayed representation at 40 characters.
pub(super) fn shown(value: &Value) -> String {
    let representation = python_repr(value);
    if representation.chars().count() <= 40 {
        representation
    } else {
        representation.chars().take(39).collect::<String>() + "…"
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(value) => string_repr(value),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|(key, value)| format!("{}: {}", string_repr(key), python_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn string_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut result = String::from(quote);
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            character if character == quote => {
                result.push('\\');
                result.push(character);
            }
            character if escaped(character) => {
                let code = character as u32;
                if code <= 0xff {
                    result.push_str(&format!("\\x{code:02x}"));
                } else if code <= 0xffff {
                    result.push_str(&format!("\\u{code:04x}"));
                } else {
                    result.push_str(&format!("\\U{code:08x}"));
                }
            }
            character => result.push(character),
        }
    }
    result.push(quote);
    result
}

/// Characters Python does not print as themselves; combining marks and skin-tone modifiers stay literal.
fn escaped(character: char) -> bool {
    character.is_control()
        || (character.is_whitespace() && character != ' ')
        || (character.escape_debug().to_string().starts_with("\\u{")
            && !is_combining_mark(character)
            && !('\u{1f3fb}'..='\u{1f3ff}').contains(&character))
}
