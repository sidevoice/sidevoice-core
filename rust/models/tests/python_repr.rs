//! Python `repr()` display of refused values.

use serde_json::json;

use crate::models::python_repr::shown;

#[test]
fn shown_matches_python_repr_at_the_display_limit() {
    assert_eq!(
        shown(&json!("x".repeat(38))),
        format!("'{}'", "x".repeat(38))
    );
    assert_eq!(
        shown(&json!("x".repeat(39))),
        format!("'{}…", "x".repeat(38))
    );
    assert_eq!(shown(&json!("a'b\"c")), "'a\\'b\"c'");
    assert_eq!(shown(&json!("a\\b")), "'a\\\\b'");
    assert_eq!(shown(&json!("\u{200b}")), "'\\u200b'");
    assert_eq!(shown(&json!("\u{e000}")), "'\\ue000'");
    assert_eq!(shown(&json!("\u{301}")), "'\u{301}'");
    assert_eq!(shown(&json!("\u{1f3fb}")), "'\u{1f3fb}'");
}
