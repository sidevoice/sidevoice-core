//! OpenAI speech-to-text: the transcription model catalog, transcription and key verification.

mod client;
mod retry;
mod wire;

use super::ProviderError;
use async_openai::{
    config::OpenAIConfig,
    types::audio::{AudioInput, CreateTranscriptionRequest},
    Client,
};
use std::time::Duration;

use client::build_client;
use wire::{is_transcription_model, map_openai_error, parse_transcription, response_shape};

const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRIES: usize = 2;

pub struct OpenAiTranscriber {
    client: Client<OpenAIConfig>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Transcription {
    pub text: String,
    pub mean_logprob: Option<f64>,
}

impl OpenAiTranscriber {
    pub fn new(api_key: &str) -> Result<Self, ProviderError> {
        let base = api_base();
        Ok(Self {
            client: build_client(
                api_key,
                &base,
                TRANSCRIPTION_TIMEOUT,
                CONNECT_TIMEOUT,
                MAX_RETRIES,
            )?,
        })
    }

    pub async fn catalog(&self) -> Result<Vec<String>, ProviderError> {
        let response = self
            .client
            .models()
            .list()
            .await
            .map_err(map_openai_error)?;
        let mut models: Vec<String> = response
            .data
            .into_iter()
            .map(|model| model.id)
            .filter(|id| is_transcription_model(id))
            .collect();
        models.sort_by_key(|id| (id != "gpt-4o-transcribe", id.clone()));
        Ok(models)
    }

    pub async fn transcribe(
        &self,
        wav: &[u8],
        model: &str,
        language: Option<&str>,
        prompt: Option<&str>,
    ) -> Result<Transcription, ProviderError> {
        let (response_format, include) = response_shape(model);
        let request = CreateTranscriptionRequest {
            file: AudioInput::from_vec_u8("audio.wav".to_owned(), wav.to_vec()),
            model: model.to_owned(),
            language: language.map(str::to_owned),
            prompt: prompt.map(str::to_owned),
            response_format,
            include,
            ..CreateTranscriptionRequest::default()
        };
        // create_raw still uses async-openai's typed audio request and replayable multipart pipeline.
        // Its byte-input helper preserves `audio.wav` but does not attach a part MIME header; the
        // OpenAI transcription contract accepts WAV and recommends an extension-bearing filename
        // plus a content type, so the extension remains the identifying format metadata here.
        let body = self
            .client
            .audio()
            .transcription()
            .create_raw(request)
            .await
            .map_err(map_openai_error)?;
        parse_transcription(&body)
    }
}

pub async fn verify_openai_key(api_key: &str) -> Result<(), ProviderError> {
    let base = api_base();
    let client = build_client(api_key, &base, VERIFY_TIMEOUT, VERIFY_TIMEOUT, 0)?;
    verify_client(&client).await?;
    Ok(())
}

fn api_base() -> String {
    #[cfg(feature = "hosted-fixtures")]
    if let Ok(base) = std::env::var("SIDEVOICE_OPENAI_FIXTURE_BASE") {
        return base;
    }
    OPENAI_API_BASE.to_owned()
}

async fn verify_client(client: &Client<OpenAIConfig>) -> Result<(), ProviderError> {
    client.models().list().await.map_err(map_openai_error)?;
    Ok(())
}

#[cfg(test)]
mod tests;
