//! The ElevenLabs model and voice catalog, with localized fallback models when it cannot be loaded.

use super::{client, wire::map_elevenlabs_error, ElevenLabsTts};
use crate::messages::{render, LocalizedMessage};
use crate::providers::ProviderError;
use elevenlabs_sdk::{
    error::ElevenLabsError,
    types::{GetModelsResponse, GetVoicesResponse, GetVoicesV2Response, Model, Voice},
    ElevenLabsClient,
};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use std::{collections::BTreeSet, time::Duration};

const CATALOG_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_VOICE_PAGES: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct ElevenLabsModel {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ElevenLabsVoice {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ElevenLabsCatalog {
    pub models: Vec<ElevenLabsModel>,
    pub voices: Vec<ElevenLabsVoice>,
    pub error: Option<ProviderError>,
}

impl ElevenLabsCatalog {
    pub fn unconfigured(language: &str) -> Self {
        Self {
            models: fallback_models(language),
            voices: Vec::new(),
            error: None,
        }
    }

    fn failed(language: &str, error: ProviderError) -> Self {
        Self {
            error: Some(error),
            ..Self::unconfigured(language)
        }
    }
}

impl ElevenLabsTts {
    pub async fn catalog(&self, language: &str) -> ElevenLabsCatalog {
        let base_url = self.client.config().base_url.clone();
        let key = self.client.config().api_key.as_str().to_owned();
        let client = match client(&key, &base_url, CATALOG_TIMEOUT) {
            Ok(client) => client,
            Err(error) => return ElevenLabsCatalog::failed(language, error),
        };
        let (models, voices) = tokio::join!(load_models(&client), load_voices(&client));
        match (models, voices) {
            (Ok(models), Ok(voices)) => ElevenLabsCatalog {
                models: if models.is_empty() {
                    fallback_models(language)
                } else {
                    models
                },
                voices,
                error: None,
            },
            (Err(error), _) | (_, Err(error)) => ElevenLabsCatalog::failed(language, error),
        }
    }
}

pub(super) async fn load_models(
    client: &ElevenLabsClient,
) -> Result<Vec<ElevenLabsModel>, ProviderError> {
    let response: GetModelsResponse = client.models().list().await.map_err(map_elevenlabs_error)?;
    Ok(response
        .0
        .into_iter()
        .filter(|model| model.can_do_text_to_speech && !model.model_id.is_empty())
        .map(model_entry)
        .collect())
}

fn model_entry(model: Model) -> ElevenLabsModel {
    let description =
        (!model.description.trim().is_empty()).then(|| model.description.trim().to_owned());
    let label = if model.name.is_empty() {
        model.model_id.clone()
    } else {
        model.name
    };
    ElevenLabsModel {
        id: model.model_id,
        label,
        description,
    }
}

pub(super) async fn load_voices(
    client: &ElevenLabsClient,
) -> Result<Vec<ElevenLabsVoice>, ProviderError> {
    let mut voices = Vec::new();
    let mut next_page_token: Option<String> = None;
    for page in 0..MAX_VOICE_PAGES {
        // elevenlabs-sdk 0.1.0 interpolates cursor text directly into the query. Encode the opaque
        // provider token before passing it through the typed SDK method.
        let encoded_token = next_page_token
            .as_deref()
            .map(|token| utf8_percent_encode(token, NON_ALPHANUMERIC).to_string());
        let response: GetVoicesV2Response = match client
            .voices()
            .get_voices_v2(encoded_token.as_deref(), Some(100), None, None, None)
            .await
        {
            Ok(response) => response,
            Err(ElevenLabsError::Api { status: 404, .. }) if page == 0 => {
                return load_legacy_voices(client).await;
            }
            Err(error) => return Err(map_elevenlabs_error(error)),
        };
        voices.extend(response.voices.into_iter().filter_map(voice_entry));
        next_page_token = response.next_page_token;
        if !response.has_more || next_page_token.as_deref().is_none_or(str::is_empty) {
            break;
        }
    }
    Ok(voices)
}

async fn load_legacy_voices(
    client: &ElevenLabsClient,
) -> Result<Vec<ElevenLabsVoice>, ProviderError> {
    let legacy: GetVoicesResponse = client
        .voices()
        .list(None)
        .await
        .map_err(map_elevenlabs_error)?;
    Ok(legacy.voices.into_iter().filter_map(voice_entry).collect())
}

fn voice_entry(voice: Voice) -> Option<ElevenLabsVoice> {
    if voice.voice_id.is_empty() {
        return None;
    }
    let languages = voice_languages(&voice);
    let category = voice
        .category
        .and_then(|category| serde_json::to_value(category).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .or_else(|| voice.voice_type.filter(|value| !value.is_empty()));
    Some(ElevenLabsVoice {
        id: voice.voice_id.clone(),
        label: if voice.name.is_empty() {
            voice.voice_id
        } else {
            voice.name
        },
        description: category,
        languages,
    })
}

/// The voice's primary language when it declares one, otherwise every verified language.
fn voice_languages(voice: &Voice) -> Vec<String> {
    let primary_language = voice
        .labels
        .get("language")
        .map(String::as_str)
        .filter(|language| !language.is_empty())
        .or(voice.language.as_deref());
    if let Some(primary) = primary_language.and_then(language_code) {
        return vec![primary];
    }
    voice
        .verified_languages
        .iter()
        .flatten()
        .filter_map(|verified| language_code(&verified.language))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn language_code(value: &str) -> Option<String> {
    let primary = value.trim().to_ascii_lowercase().replace('_', "-");
    let code = primary.split('-').next()?;
    (code.len() >= 2 && code.len() <= 3 && code.chars().all(|ch| ch.is_ascii_alphabetic()))
        .then(|| code.to_owned())
}

fn fallback_models(language: &str) -> Vec<ElevenLabsModel> {
    [
        (
            "eleven_flash_v2_5",
            "Eleven Flash v2.5",
            "catalog.elevenlabs.fast",
        ),
        (
            "eleven_multilingual_v2",
            "Eleven Multilingual v2",
            "catalog.elevenlabs.multilingual",
        ),
        ("eleven_v3", "Eleven v3", "catalog.elevenlabs.expressive"),
    ]
    .into_iter()
    .map(|(id, label, description_key)| ElevenLabsModel {
        id: id.to_owned(),
        label: label.to_owned(),
        description: Some(render(&LocalizedMessage::new(description_key), language)),
    })
    .collect()
}
