//! The provider-neutral failure classification shared by every adapter.

/// Stable provider failure classification. SDK diagnostic strings and credentials are never retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub status: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    InvalidConfiguration,
    Unauthorized,
    RateLimited,
    Http,
    Timeout,
    Transport,
    MalformedResponse,
}

impl ProviderError {
    pub(super) fn new(kind: ProviderErrorKind, status: Option<u16>) -> Self {
        Self { kind, status }
    }

    pub(super) fn from_status(status: u16) -> Self {
        let kind = match status {
            401 | 403 => ProviderErrorKind::Unauthorized,
            429 => ProviderErrorKind::RateLimited,
            _ => ProviderErrorKind::Http,
        };
        Self::new(kind, Some(status))
    }

    /// A failure of the HTTP transport itself, kept apart from other transport failures when it timed out.
    pub(super) fn transport(timed_out: bool, status: Option<u16>) -> Self {
        let kind = if timed_out {
            ProviderErrorKind::Timeout
        } else {
            ProviderErrorKind::Transport
        };
        Self::new(kind, status)
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(formatter, "provider {:?} ({status})", self.kind),
            None => write!(formatter, "provider {:?}", self.kind),
        }
    }
}

impl std::error::Error for ProviderError {}

#[cfg(test)]
mod tests;
