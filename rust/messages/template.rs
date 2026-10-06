//! Named-parameter substitution in bundle templates, with a `{name:.1f}` fixed-precision format.

use serde_json::{Map, Value};

/// Replace each `{name}` with its parameter. A missing parameter renders as nothing and an
/// unclosed brace is kept literally.
pub(super) fn interpolate(template: &str, params: &Map<String, Value>) -> String {
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
            output.push_str(&formatted(value, format_spec));
        }
        remaining = &after_open[close + 1..];
    }
    output.push_str(remaining);
    output
}

fn formatted(value: &Value, format_spec: Option<&str>) -> String {
    match (format_spec, value.as_f64()) {
        (Some(".1f"), Some(number)) => format!("{number:.1}"),
        _ => parameter(value),
    }
}

fn parameter(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Array(values) => values.iter().map(parameter).collect::<Vec<_>>().join(", "),
        value => value.to_string(),
    }
}
