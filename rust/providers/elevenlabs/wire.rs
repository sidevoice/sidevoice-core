//! ElevenLabs wire formats: the timestamped audio stream, output formats and SDK error mapping.

use crate::providers::{ProviderError, ProviderErrorKind};
use base64::{engine::general_purpose::STANDARD, Engine};
use elevenlabs_sdk::{
    error::ElevenLabsError,
    types::{CharacterAlignment, OutputFormat, StreamingAudioChunkWithTimestamps},
};
use serde_json::{json, Value};

/// Reassembles the concatenated JSON records of a timestamped stream into audio and one alignment.
pub(super) fn decode_timestamp_stream(
    body: &[u8],
) -> Result<(Vec<u8>, Option<Value>), ProviderError> {
    let mut audio = Vec::new();
    let mut alignment = Alignment::default();
    let mut alignment_seen = false;
    let chunks =
        serde_json::Deserializer::from_slice(body).into_iter::<StreamingAudioChunkWithTimestamps>();
    for chunk in chunks {
        let chunk = chunk.map_err(|_| malformed())?;
        audio.extend(
            STANDARD
                .decode(chunk.audio_base64)
                .map_err(|_| malformed())?,
        );
        if let Some(chunk_alignment) = chunk.alignment {
            alignment_seen = true;
            alignment.append(chunk_alignment);
        }
    }
    Ok((audio, alignment_seen.then(|| alignment.into_json())))
}

#[derive(Default)]
struct Alignment {
    characters: Vec<String>,
    starts: Vec<f64>,
    ends: Vec<f64>,
}

impl Alignment {
    fn append(&mut self, alignment: CharacterAlignment) {
        self.characters.extend(alignment.characters);
        self.starts.extend(alignment.character_start_times_seconds);
        self.ends.extend(alignment.character_end_times_seconds);
    }

    fn into_json(self) -> Value {
        json!({
            "characters": self.characters,
            "character_start_times_seconds": self.starts,
            "character_end_times_seconds": self.ends,
        })
    }
}

pub(super) fn output_format_from_str(value: &str) -> Result<OutputFormat, ProviderError> {
    serde_json::from_value(Value::String(value.to_owned()))
        .map_err(|_| ProviderError::new(ProviderErrorKind::InvalidConfiguration, None))
}

pub(super) fn mime_type(format: OutputFormat) -> String {
    let format = format.to_string();
    if format.starts_with("mp3") {
        "audio/mpeg".to_owned()
    } else if format.starts_with("pcm") {
        "audio/pcm".to_owned()
    } else {
        "application/octet-stream".to_owned()
    }
}

pub(super) fn map_elevenlabs_error(error: ElevenLabsError) -> ProviderError {
    match error {
        ElevenLabsError::Api { status, .. } => ProviderError::from_status(status),
        ElevenLabsError::Auth(_) => ProviderError::new(ProviderErrorKind::Unauthorized, Some(401)),
        ElevenLabsError::RateLimited { .. } => {
            ProviderError::new(ProviderErrorKind::RateLimited, Some(429))
        }
        ElevenLabsError::Timeout => ProviderError::new(ProviderErrorKind::Timeout, None),
        ElevenLabsError::Transport(_) | ElevenLabsError::WebSocket(_) => {
            ProviderError::new(ProviderErrorKind::Transport, None)
        }
        ElevenLabsError::Deserialization(_) => malformed(),
        ElevenLabsError::Validation(_) | ElevenLabsError::InvalidUrl(_) => {
            ProviderError::new(ProviderErrorKind::InvalidConfiguration, None)
        }
    }
}

/// Maps a failure while reading a response body, where only a timeout is told apart.
///
/// The SDK yields its HTTP client's error type, which this crate reaches only through the SDK's
/// `Transport` conversion.
pub(super) fn stream_error(error: impl Into<ElevenLabsError>) -> ProviderError {
    let error: ElevenLabsError = error.into();
    let timed_out = matches!(error, ElevenLabsError::Transport(error) if error.is_timeout());
    ProviderError::transport(timed_out, None)
}

fn malformed() -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedResponse, None)
}
