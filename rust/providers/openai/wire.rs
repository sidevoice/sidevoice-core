//! OpenAI wire shapes: which models transcribe, the requested response format, the response
//! fields that are read, and SDK error mapping.

use super::Transcription;
use crate::providers::{ProviderError, ProviderErrorKind};
use async_openai::{
    error::OpenAIError,
    types::audio::{AudioResponseFormat, TranscriptionInclude},
};
use serde_json::Value;

/// Whether a listed model id is a non-realtime transcription model with a safe id.
pub(super) fn is_transcription_model(id: &str) -> bool {
    (id == "whisper-1"
        || (id.contains("transcribe") && !id.contains("realtime") && !id.contains("live")))
        && id.len() <= 120
        && id
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "._:-".contains(ch))
}

/// The response format to request for a model, and the logprobs to ask for with it.
pub(super) fn response_shape(
    model: &str,
) -> (
    Option<AudioResponseFormat>,
    Option<Vec<TranscriptionInclude>>,
) {
    let response_format = if model.starts_with("whisper") {
        Some(AudioResponseFormat::VerboseJson)
    } else if model.contains("diarize") {
        None
    } else {
        Some(AudioResponseFormat::Json)
    };
    let include = (response_format == Some(AudioResponseFormat::Json))
        .then_some(vec![TranscriptionInclude::Logprobs]);
    (response_format, include)
}

/// Parses only the response fields the core consumes, so null token logprobs are accepted.
pub(super) fn parse_transcription(body: &[u8]) -> Result<Transcription, ProviderError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ProviderError::new(ProviderErrorKind::MalformedResponse, None))?;
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let probabilities: Vec<f64> = value
        .get("logprobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("logprob").and_then(Value::as_f64))
        .collect();
    let mean_logprob = (!probabilities.is_empty())
        .then(|| probabilities.iter().sum::<f64>() / probabilities.len() as f64);
    Ok(Transcription { text, mean_logprob })
}

pub(super) fn map_openai_error(error: OpenAIError) -> ProviderError {
    match error {
        OpenAIError::ApiError(response) => {
            ProviderError::from_status(response.status_code.as_u16())
        }
        OpenAIError::Reqwest(error) => ProviderError::transport(
            error.is_timeout(),
            error.status().map(|status| status.as_u16()),
        ),
        OpenAIError::JSONDeserialize(_, _) => {
            ProviderError::new(ProviderErrorKind::MalformedResponse, None)
        }
        OpenAIError::InvalidArgument(_) => {
            ProviderError::new(ProviderErrorKind::InvalidConfiguration, None)
        }
        OpenAIError::StreamError(_) | OpenAIError::Boxed(_) => {
            ProviderError::new(ProviderErrorKind::Transport, None)
        }
        #[cfg(not(target_family = "wasm"))]
        OpenAIError::FileReadError(_) | OpenAIError::FileSaveError(_) => {
            ProviderError::new(ProviderErrorKind::InvalidConfiguration, None)
        }
    }
}
