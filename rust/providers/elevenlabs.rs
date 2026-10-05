//! ElevenLabs text-to-speech: one SDK client per purpose, the voice/model catalog, and synthesis.

mod catalog;
mod synthesis;
mod wire;

use super::{ProviderError, ProviderErrorKind};
use elevenlabs_sdk::{ClientConfig, ElevenLabsClient};
use std::time::Duration;

pub use catalog::{ElevenLabsCatalog, ElevenLabsModel, ElevenLabsVoice};
pub use synthesis::CloudSpeech;

const ELEVENLABS_API_BASE: &str = "https://api.elevenlabs.io";
const SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(45);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ElevenLabsTts {
    client: ElevenLabsClient,
}

impl ElevenLabsTts {
    pub fn new(api_key: &str) -> Result<Self, ProviderError> {
        #[cfg(feature = "hosted-fixtures")]
        if let Ok(base_url) = std::env::var("SIDEVOICE_ELEVENLABS_FIXTURE_BASE") {
            return Self::with_config(api_key, &base_url, SYNTHESIS_TIMEOUT);
        }
        Self::with_config(api_key, ELEVENLABS_API_BASE, SYNTHESIS_TIMEOUT)
    }

    fn with_config(
        api_key: &str,
        base_url: &str,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            client: client(api_key, base_url, timeout)?,
        })
    }
}

pub async fn verify_elevenlabs_key(api_key: &str) -> Result<(), ProviderError> {
    #[cfg(feature = "hosted-fixtures")]
    let base = std::env::var("SIDEVOICE_ELEVENLABS_FIXTURE_BASE")
        .unwrap_or_else(|_| ELEVENLABS_API_BASE.to_owned());
    #[cfg(not(feature = "hosted-fixtures"))]
    let base = ELEVENLABS_API_BASE.to_owned();
    let client = client(api_key, &base, VERIFY_TIMEOUT)?;
    verify_client(&client).await
}

async fn verify_client(client: &ElevenLabsClient) -> Result<(), ProviderError> {
    catalog::load_models(client).await?;
    catalog::load_voices(client).await?;
    Ok(())
}

fn client(
    api_key: &str,
    base_url: &str,
    timeout: Duration,
) -> Result<ElevenLabsClient, ProviderError> {
    if api_key.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidConfiguration,
            None,
        ));
    }
    let config = ClientConfig::builder(api_key)
        .base_url(base_url)
        .timeout(timeout)
        .max_retries(0)
        .build();
    ElevenLabsClient::new(config).map_err(wire::map_elevenlabs_error)
}

#[cfg(test)]
mod tests;
