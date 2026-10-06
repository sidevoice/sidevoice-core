//! Provider key precedence and non-secret hints.

use crate::models::{credential_state, effective_key, CredentialState};

#[test]
fn key_precedence_and_hints_never_return_a_secret() {
    assert_eq!(
        effective_key(Some("  saved-secret "), Some("environment-secret")),
        Some("saved-secret")
    );
    assert_eq!(
        effective_key(Some("  "), Some(" env-secret ")),
        Some("env-secret")
    );
    assert_eq!(effective_key(None, Some(" \n ")), None);
    let stored = credential_state(Some(" secret-1234 "), Some("environment"));
    assert_eq!(
        stored,
        CredentialState {
            configured: true,
            source: Some("stored"),
            hint: Some("…1234".to_owned())
        }
    );
    let environment = credential_state(None, Some("env-5678"));
    assert_eq!(environment.source, Some("environment"));
    assert_eq!(environment.hint.as_deref(), Some("…5678"));
    assert_eq!(credential_state(None, None).hint, None);
}
