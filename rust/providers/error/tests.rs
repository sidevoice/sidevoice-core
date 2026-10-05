use super::{ProviderError, ProviderErrorKind};

#[test]
fn status_classification_separates_auth_and_rate_limits_from_other_http_failures() {
    for (status, kind) in [
        (401, ProviderErrorKind::Unauthorized),
        (403, ProviderErrorKind::Unauthorized),
        (429, ProviderErrorKind::RateLimited),
        (404, ProviderErrorKind::Http),
        (500, ProviderErrorKind::Http),
    ] {
        assert_eq!(
            ProviderError::from_status(status),
            ProviderError::new(kind, Some(status))
        );
    }
}

#[test]
fn transport_failures_keep_timeouts_distinct_and_carry_the_status() {
    assert_eq!(
        ProviderError::transport(true, None),
        ProviderError::new(ProviderErrorKind::Timeout, None)
    );
    assert_eq!(
        ProviderError::transport(false, Some(502)),
        ProviderError::new(ProviderErrorKind::Transport, Some(502))
    );
}

#[test]
fn display_names_only_the_kind_and_status() {
    assert_eq!(
        ProviderError::from_status(429).to_string(),
        "provider RateLimited (429)"
    );
    assert_eq!(
        ProviderError::new(ProviderErrorKind::Timeout, None).to_string(),
        "provider Timeout"
    );
}
