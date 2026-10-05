//! Provider key precedence and the non-secret view of a configured key.

use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CredentialState {
    pub configured: bool,
    pub source: Option<&'static str>,
    pub hint: Option<String>,
}

/// A saved provider key wins; an empty/whitespace value is treated as absent.
pub fn effective_key<'a>(stored: Option<&'a str>, environment: Option<&'a str>) -> Option<&'a str> {
    present(stored).or_else(|| present(environment))
}

/// Return only configuration state and a last-four-character hint, never the key itself.
pub fn credential_state(stored: Option<&str>, environment: Option<&str>) -> CredentialState {
    let (key, source) = if let Some(key) = present(stored) {
        (Some(key), Some("stored"))
    } else if let Some(key) = present(environment) {
        (Some(key), Some("environment"))
    } else {
        (None, None)
    };
    let hint = key.map(|key| {
        let mut suffix = key.chars().rev().take(4).collect::<String>();
        suffix = suffix.chars().rev().collect();
        format!("…{suffix}")
    });
    CredentialState {
        configured: key.is_some(),
        source,
        hint,
    }
}

fn present(key: Option<&str>) -> Option<&str> {
    key.map(str::trim).filter(|value| !value.is_empty())
}
