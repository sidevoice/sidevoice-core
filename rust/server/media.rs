//! One microphone source and one turn lifecycle for an authenticated call.

mod audio;
mod call;
mod keys;
mod recognition;
mod speech;
mod transcripts;
mod turns;

#[cfg(test)]
mod tests;

pub(super) use audio::wav;
pub(super) use call::{CallMedia, Source};
pub(super) use keys::provider_key;
pub(super) use speech::speech_event;
pub(super) use turns::TurnOwner;
