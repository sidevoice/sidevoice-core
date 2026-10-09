//! Settings, the models a device reports, and model-check rules.
//!
//! The provider and speech-language catalogues are embedded data; local models are what each device reports.
//! Each submodule owns one set of rules.

mod availability;
mod catalog;
mod check_fixtures;
mod check_verdicts;
mod credentials;
mod defaults;
mod device_models;
mod json;
mod mic;
mod options;
mod runtime;
mod settings;
mod stage;
mod stage_diagnostics;
mod voice;

pub use availability::unavailable;
pub use catalog::voice_languages;
pub use check_fixtures::{check_language, stt_check_clip, tts_check_phrase, CheckClip};
pub use check_verdicts::{audio_problem, slow, transcript_problem, word_error};
pub use credentials::{credential_state, effective_key, CredentialState};
pub use defaults::default_settings;
pub use device_models::device_models;
pub use mic::{mic_settings, MicSettings};
pub use runtime::{browser_runtime, call_transcription};
pub use settings::{settings_from, SettingsLoad};
pub use stage::provider_check_stage;
pub use voice::{resolve_voice, ResolvedVoice};

#[cfg(test)]
mod tests;
