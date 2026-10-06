//! Stateless cloud-provider SDK adapters and the bounded shared synthesis cache.

pub mod cache;
mod elevenlabs;
mod error;
mod openai;

pub use elevenlabs::{
    verify_elevenlabs_key, CloudSpeech, ElevenLabsCatalog, ElevenLabsModel, ElevenLabsTts,
    ElevenLabsVoice,
};
pub use error::{ProviderError, ProviderErrorKind};
pub use openai::{verify_openai_key, OpenAiTranscriber, Transcription};
