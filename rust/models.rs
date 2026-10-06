//! Catalogue, settings and model-check rules shared with the existing Python vectors.
//!
//! The catalogue is embedded data; each submodule owns one set of rules over it.

mod availability;
mod catalog;
mod catalog_problems;
mod check_fixtures;
mod check_verdicts;
mod credentials;
mod defaults;
mod json;
mod mic;
mod offers;
mod options;
mod python_repr;
mod runtime;
mod settings;
mod stage;
mod stage_diagnostics;
mod voice;

pub use availability::unavailable;
pub use catalog::{catalog, catalog_text, voice_languages};
pub use catalog_problems::catalog_problems;
pub use check_fixtures::{check_language, stt_check_clip, tts_check_phrase, CheckClip};
pub use check_verdicts::{audio_problem, slow, transcript_problem, word_error};
pub use credentials::{credential_state, effective_key, CredentialState};
pub use defaults::default_settings;
pub use mic::{mic_settings, MicSettings};
pub use offers::{offers, BuildAlternative, ModelOffer, UnknownPlace};
pub use runtime::{browser_runtime, call_transcription};
pub use settings::{settings_from, SettingsLoad};
pub use stage::provider_check_stage;
pub use voice::{resolve_voice, ResolvedVoice};

#[cfg(test)]
mod tests;
