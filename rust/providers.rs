//! Stateless cloud-provider SDK adapters and the bounded shared synthesis cache.

pub mod cache;
mod elevenlabs;
mod openai;

pub use elevenlabs::{
    verify_elevenlabs_key, CloudSpeech, ElevenLabsCatalog, ElevenLabsModel, ElevenLabsTts,
    ElevenLabsVoice,
};
pub use openai::{verify_openai_key, OpenAiTranscriber, Transcription};

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
    pub(crate) fn new(kind: ProviderErrorKind, status: Option<u16>) -> Self {
        Self { kind, status }
    }

    pub(crate) fn from_status(status: u16) -> Self {
        let kind = match status {
            401 | 403 => ProviderErrorKind::Unauthorized,
            429 => ProviderErrorKind::RateLimited,
            _ => ProviderErrorKind::Http,
        };
        Self::new(kind, Some(status))
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

pub(crate) fn elapsed_ms(start: tokio::time::Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}
