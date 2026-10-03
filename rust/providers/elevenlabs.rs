use super::{elapsed_ms, ProviderError, ProviderErrorKind};
use crate::messages::{render, LocalizedMessage};
use base64::{engine::general_purpose::STANDARD, Engine};
use elevenlabs_sdk::{
    error::ElevenLabsError,
    types::{
        CharacterAlignment, GetModelsResponse, GetVoicesResponse, GetVoicesV2Response, Model,
        OutputFormat, StreamingAudioChunkWithTimestamps, TextToSpeechRequest, Voice, VoiceSettings,
    },
    ClientConfig, ElevenLabsClient,
};
use futures_util::StreamExt;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};
use std::{collections::BTreeSet, time::Duration};
use tokio::time::Instant;

const ELEVENLABS_API_BASE: &str = "https://api.elevenlabs.io";
const SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(45);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(12);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_VOICE_PAGES: usize = 20;

pub struct ElevenLabsTts {
    client: ElevenLabsClient,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CloudSpeech {
    pub audio: Vec<u8>,
    pub mime_type: String,
    pub alignment: Option<Value>,
    pub timings_ms: Map<String, Value>,
}

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

impl ElevenLabsTts {
    pub fn new(api_key: &str) -> Result<Self, ProviderError> {
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

    pub async fn catalog(&self, language: &str) -> ElevenLabsCatalog {
        let base_url = self.client.config().base_url.clone();
        let key = self.client.config().api_key.as_str().to_owned();
        let client = match client(&key, &base_url, CATALOG_TIMEOUT) {
            Ok(client) => client,
            Err(error) => {
                return ElevenLabsCatalog {
                    models: fallback_models(language),
                    voices: Vec::new(),
                    error: Some(error),
                }
            }
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
            (Err(error), _) | (_, Err(error)) => ElevenLabsCatalog {
                models: fallback_models(language),
                voices: Vec::new(),
                error: Some(error),
            },
        }
    }

    pub async fn synthesize(
        &self,
        text: &str,
        model: &str,
        voice: &str,
        speed: f64,
        with_timestamps: bool,
        output_format: &str,
    ) -> Result<CloudSpeech, ProviderError> {
        if !speed.is_finite() || voice.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidConfiguration,
                None,
            ));
        }
        let output_format = output_format_from_str(output_format)?;
        let request = TextToSpeechRequest {
            text: text.to_owned(),
            model_id: Some(model.to_owned()),
            voice_settings: Some(VoiceSettings {
                stability: None,
                similarity_boost: None,
                style: None,
                use_speaker_boost: None,
                speed: Some(speed.clamp(0.7, 1.2)),
            }),
            ..TextToSpeechRequest::new("")
        };
        let encoded_voice = utf8_percent_encode(voice, NON_ALPHANUMERIC).to_string();
        let started = Instant::now();
        let mut timings_ms = Map::new();
        let (audio, alignment) = if with_timestamps {
            let service = self.client.text_to_speech();
            let mut stream = service
                .convert_stream_with_timestamps(&encoded_voice, &request, Some(output_format), None)
                .await
                .map_err(map_elevenlabs_error)?;
            // These are observations at the SDK stream boundary: method return after successful
            // headers, first nonempty Bytes item, and stream exhaustion. hpx may buffer or coalesce
            // body data differently from Python aiohttp's iter_any(), so first-byte parity is not
            // guaranteed even though the public timing key and observed event are retained.
            timings_ms.insert(
                "request_to_headers_ms".to_owned(),
                json!(elapsed_ms(started)),
            );
            let mut body = Vec::new();
            let mut first_chunk_recorded = false;
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|_| ProviderError::new(ProviderErrorKind::Transport, None))?;
                if !chunk.is_empty() {
                    if !first_chunk_recorded {
                        first_chunk_recorded = true;
                        timings_ms.insert(
                            "request_to_first_chunk_ms".to_owned(),
                            json!(elapsed_ms(started)),
                        );
                    }
                    body.extend_from_slice(&chunk);
                }
            }
            timings_ms.insert(
                "request_to_complete_ms".to_owned(),
                json!(elapsed_ms(started)),
            );
            decode_timestamp_stream(&body)?
        } else {
            let service = self.client.text_to_speech();
            let mut stream = service
                .convert_stream(&encoded_voice, &request, Some(output_format), None)
                .await
                .map_err(map_elevenlabs_error)?;
            timings_ms.insert(
                "request_to_headers_ms".to_owned(),
                json!(elapsed_ms(started)),
            );
            let mut audio = Vec::new();
            let mut first_chunk_recorded = false;
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|_| ProviderError::new(ProviderErrorKind::Transport, None))?;
                if !chunk.is_empty() {
                    if !first_chunk_recorded {
                        first_chunk_recorded = true;
                        timings_ms.insert(
                            "request_to_first_chunk_ms".to_owned(),
                            json!(elapsed_ms(started)),
                        );
                    }
                    audio.extend_from_slice(&chunk);
                }
            }
            timings_ms.insert(
                "request_to_complete_ms".to_owned(),
                json!(elapsed_ms(started)),
            );
            (audio, None)
        };
        if audio.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::MalformedResponse,
                None,
            ));
        }
        Ok(CloudSpeech {
            audio,
            mime_type: mime_type(output_format),
            alignment,
            timings_ms,
        })
    }
}

pub async fn verify_elevenlabs_key(api_key: &str) -> Result<(), ProviderError> {
    let client = client(api_key, ELEVENLABS_API_BASE, VERIFY_TIMEOUT)?;
    verify_client(&client).await
}

async fn verify_client(client: &ElevenLabsClient) -> Result<(), ProviderError> {
    load_models(client).await?;
    load_voices(client).await?;
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
    ElevenLabsClient::new(config).map_err(map_elevenlabs_error)
}

async fn load_models(client: &ElevenLabsClient) -> Result<Vec<ElevenLabsModel>, ProviderError> {
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
    ElevenLabsModel {
        id: model.model_id.clone(),
        label: if model.name.is_empty() {
            model.model_id
        } else {
            model.name
        },
        description,
    }
}

async fn load_voices(client: &ElevenLabsClient) -> Result<Vec<ElevenLabsVoice>, ProviderError> {
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
                let legacy: GetVoicesResponse = client
                    .voices()
                    .list(None)
                    .await
                    .map_err(map_elevenlabs_error)?;
                return Ok(legacy.voices.into_iter().filter_map(voice_entry).collect());
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

fn voice_entry(voice: Voice) -> Option<ElevenLabsVoice> {
    if voice.voice_id.is_empty() {
        return None;
    }
    let primary = voice
        .labels
        .get("language")
        .and_then(|language| language_code(language));
    let languages = if let Some(primary) = primary {
        vec![primary]
    } else {
        voice
            .verified_languages
            .unwrap_or_default()
            .into_iter()
            .filter_map(|verified| language_code(&verified.language))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let category = serde_json::to_value(voice.category)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
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

fn decode_timestamp_stream(body: &[u8]) -> Result<(Vec<u8>, Option<Value>), ProviderError> {
    let mut audio = Vec::new();
    let mut alignment_seen = false;
    let mut characters = Vec::new();
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let chunks =
        serde_json::Deserializer::from_slice(body).into_iter::<StreamingAudioChunkWithTimestamps>();
    for chunk in chunks {
        let chunk =
            chunk.map_err(|_| ProviderError::new(ProviderErrorKind::MalformedResponse, None))?;
        audio.extend(
            STANDARD
                .decode(chunk.audio_base64)
                .map_err(|_| ProviderError::new(ProviderErrorKind::MalformedResponse, None))?,
        );
        if let Some(alignment) = chunk.alignment {
            alignment_seen = true;
            append_alignment(&mut characters, &mut starts, &mut ends, alignment);
        }
    }
    let alignment = alignment_seen.then(|| {
        json!({
            "characters": characters,
            "character_start_times_seconds": starts,
            "character_end_times_seconds": ends,
        })
    });
    Ok((audio, alignment))
}

fn append_alignment(
    characters: &mut Vec<String>,
    starts: &mut Vec<f64>,
    ends: &mut Vec<f64>,
    alignment: CharacterAlignment,
) {
    characters.extend(alignment.characters);
    starts.extend(alignment.character_start_times_seconds);
    ends.extend(alignment.character_end_times_seconds);
}

fn output_format_from_str(value: &str) -> Result<OutputFormat, ProviderError> {
    serde_json::from_value(Value::String(value.to_owned()))
        .map_err(|_| ProviderError::new(ProviderErrorKind::InvalidConfiguration, None))
}

fn mime_type(format: OutputFormat) -> String {
    let format = format.to_string();
    if format.starts_with("mp3") {
        "audio/mpeg".to_owned()
    } else if format.starts_with("pcm") {
        "audio/pcm".to_owned()
    } else {
        "application/octet-stream".to_owned()
    }
}

fn map_elevenlabs_error(error: ElevenLabsError) -> ProviderError {
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
        ElevenLabsError::Deserialization(_) => {
            ProviderError::new(ProviderErrorKind::MalformedResponse, None)
        }
        ElevenLabsError::Validation(_) | ElevenLabsError::InvalidUrl(_) => {
            ProviderError::new(ProviderErrorKind::InvalidConfiguration, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{verify_client, CloudSpeech, ElevenLabsTts};
    use crate::providers::ProviderErrorKind;
    use serde_json::Value;
    use std::{net::SocketAddr, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };
    use wiremock::{
        matchers::{header, method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    const MODELS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/eleven_models.json"
    ));
    const VOICES_ONE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/eleven_voices_page_one.json"
    ));
    const VOICES_TWO: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/eleven_voices_page_two.json"
    ));
    const LEGACY_VOICES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/eleven_voices_legacy.json"
    ));
    const TIMESTAMPS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/eleven_timestamps.json"
    ));
    const TIMESTAMP_CHUNKS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/eleven_timestamp_chunks.json"
    ));

    fn local_tts(server: &MockServer, timeout: Duration) -> ElevenLabsTts {
        ElevenLabsTts::with_config("test-key", &server.uri(), timeout).unwrap()
    }

    async fn split_timestamp_server() -> (String, JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        let base_url = format!("http://{address}");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0, "SDK closed before sending request headers");
                request.extend_from_slice(&buffer[..read]);
            }

            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(60)).await;

            let body = TIMESTAMP_CHUNKS.as_bytes();
            let first_record_end = body.iter().position(|byte| *byte == b'\n').unwrap() + 1;
            let first_fragment_end = 17;
            write_http_chunk(&mut socket, &body[..first_fragment_end]).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            write_http_chunk(&mut socket, &body[first_fragment_end..first_record_end]).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            write_http_chunk(&mut socket, &body[first_record_end..]).await;
            socket.write_all(b"0\r\n\r\n").await.unwrap();
            request
        });
        (base_url, task)
    }

    async fn write_http_chunk(socket: &mut tokio::net::TcpStream, bytes: &[u8]) {
        socket
            .write_all(format!("{:X}\r\n", bytes.len()).as_bytes())
            .await
            .unwrap();
        socket.write_all(bytes).await.unwrap();
        socket.write_all(b"\r\n").await.unwrap();
    }

    async fn mount_models(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("xi-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MODELS))
            .mount(server)
            .await;
    }

    fn assert_audio(speech: &CloudSpeech, expected: &[u8]) {
        assert_eq!(speech.audio, expected);
        assert_eq!(speech.mime_type, "audio/mpeg");
    }

    #[tokio::test]
    async fn timestamp_synthesis_uses_typed_sdk_stream_and_preserves_all_python_timings() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice123/stream/with-timestamps"))
            .and(query_param("output_format", "mp3_44100_128"))
            .and(header("xi-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TIMESTAMP_CHUNKS))
            .mount(&server)
            .await;
        let result = local_tts(&server, Duration::from_secs(3))
            .synthesize(
                "Hi",
                "eleven_multilingual_v2",
                "voice123",
                1.3,
                true,
                "mp3_44100_128",
            )
            .await
            .unwrap();
        assert_audio(&result, &[1, 2, 3, 4]);
        assert_eq!(
            result.alignment.as_ref().unwrap(),
            &serde_json::json!({
                "characters": ["H", "i"],
                "character_start_times_seconds": [0.0, 0.1],
                "character_end_times_seconds": [0.1, 0.2]
            })
        );
        for key in [
            "request_to_headers_ms",
            "request_to_first_chunk_ms",
            "request_to_complete_ms",
        ] {
            assert!(result.timings_ms.contains_key(key), "missing timing {key}");
        }
        let headers = result.timings_ms["request_to_headers_ms"].as_f64().unwrap();
        let first = result.timings_ms["request_to_first_chunk_ms"]
            .as_f64()
            .unwrap();
        let complete = result.timings_ms["request_to_complete_ms"]
            .as_f64()
            .unwrap();
        assert!(headers <= first && first <= complete);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = requests[0].body_json().unwrap();
        assert_eq!(body["text"], "Hi");
        assert_eq!(body["model_id"], "eleven_multilingual_v2");
        assert_eq!(body["voice_settings"], serde_json::json!({"speed":1.2}));
    }

    #[tokio::test]
    async fn timestamp_first_chunk_measures_sdk_bytes_before_json_reassembly() {
        let (base_url, server) = split_timestamp_server().await;
        let tts =
            ElevenLabsTts::with_config("test-key", &base_url, Duration::from_secs(3)).unwrap();
        let speech = tts
            .synthesize(
                "Hi",
                "eleven_multilingual_v2",
                "voice123",
                1.0,
                true,
                "mp3_44100_128",
            )
            .await
            .unwrap();
        let request = server.await.unwrap();
        let request_headers = String::from_utf8_lossy(&request);
        assert!(
            request_headers.starts_with("POST /v1/text-to-speech/voice123/stream/with-timestamps?")
        );
        assert_audio(&speech, &[1, 2, 3, 4]);
        assert_eq!(
            speech.alignment.as_ref().unwrap()["characters"],
            serde_json::json!(["H", "i"])
        );
        let headers = speech.timings_ms["request_to_headers_ms"].as_f64().unwrap();
        let first_sdk_bytes = speech.timings_ms["request_to_first_chunk_ms"]
            .as_f64()
            .unwrap();
        let complete = speech.timings_ms["request_to_complete_ms"]
            .as_f64()
            .unwrap();
        assert!(headers < first_sdk_bytes);
        assert!(first_sdk_bytes < complete);
        assert!(complete - first_sdk_bytes >= 40.0);
    }

    #[tokio::test]
    async fn streaming_synthesis_preserves_header_first_chunk_and_complete_timings() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice123/stream"))
            .and(query_param("output_format", "pcm_16000"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"pcm-fixture", "audio/pcm"))
            .mount(&server)
            .await;
        let result = local_tts(&server, Duration::from_secs(3))
            .synthesize(
                "Check",
                "eleven_flash_v2_5",
                "voice123",
                0.4,
                false,
                "pcm_16000",
            )
            .await
            .unwrap();
        assert_eq!(result.audio, b"pcm-fixture");
        assert_eq!(result.mime_type, "audio/pcm");
        assert!(result.alignment.is_none());
        let headers = result.timings_ms["request_to_headers_ms"].as_f64().unwrap();
        let first = result.timings_ms["request_to_first_chunk_ms"]
            .as_f64()
            .unwrap();
        let complete = result.timings_ms["request_to_complete_ms"]
            .as_f64()
            .unwrap();
        assert!(headers <= first && first <= complete);
        let request = &server.received_requests().await.unwrap()[0];
        let body: Value = request.body_json().unwrap();
        assert_eq!(body["voice_settings"], serde_json::json!({"speed":0.7}));
    }

    #[tokio::test]
    async fn catalog_filters_models_maps_voices_and_encodes_opaque_pagination() {
        let server = MockServer::start().await;
        mount_models(&server).await;
        Mock::given(method("GET"))
            .and(path("/v2/voices"))
            .and(query_param("page_size", "100"))
            .and(query_param("next_page_token", "next/2+token="))
            .respond_with(ResponseTemplate::new(200).set_body_string(VOICES_TWO))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/voices"))
            .and(query_param("page_size", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VOICES_ONE))
            .mount(&server)
            .await;

        let catalog = local_tts(&server, Duration::from_secs(3))
            .catalog("en")
            .await;
        assert_eq!(catalog.error, None);
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "eleven_multilingual_v2");
        assert_eq!(
            catalog.models[0].description.as_deref(),
            Some("Multilingual model.")
        );
        assert_eq!(catalog.voices.len(), 2);
        assert_eq!(catalog.voices[0].languages, ["es"]);
        assert_eq!(catalog.voices[1].languages, ["en", "es"]);
        let requests = server.received_requests().await.unwrap();
        assert!(requests.iter().any(|request| {
            request
                .url
                .query_pairs()
                .any(|(key, value)| key == "next_page_token" && value == "next/2+token=")
        }));
    }

    #[tokio::test]
    async fn first_page_not_found_falls_back_to_legacy_voice_list() {
        let server = MockServer::start().await;
        mount_models(&server).await;
        Mock::given(method("GET"))
            .and(path("/v2/voices"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/voices"))
            .and(header("xi-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LEGACY_VOICES))
            .mount(&server)
            .await;
        let catalog = local_tts(&server, Duration::from_secs(3))
            .catalog("es")
            .await;
        assert_eq!(catalog.error, None);
        assert_eq!(catalog.voices.len(), 1);
        assert_eq!(catalog.voices[0].id, "legacy-voice");
        assert_eq!(catalog.voices[0].languages, ["en"]);
        let requests = server.received_requests().await.unwrap();
        assert!(requests
            .iter()
            .any(|request| request.url.path() == "/v1/voices"));
    }

    #[tokio::test]
    async fn key_verification_uses_only_models_and_voice_listing() {
        let server = MockServer::start().await;
        mount_models(&server).await;
        Mock::given(method("GET"))
            .and(path("/v2/voices"))
            .and(query_param("page_size", "100"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"voices":[],"has_more":false,"total_count":0}"#),
            )
            .mount(&server)
            .await;
        let client = super::client("test-key", &server.uri(), Duration::from_secs(3)).unwrap();
        verify_client(&client).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.method == "GET"));
        assert!(requests.iter().all(|request| {
            request
                .headers
                .get("xi-api-key")
                .is_some_and(|value| value == "test-key")
        }));
        assert!(requests
            .iter()
            .all(|request| !request.url.path().contains("text-to-speech")));
    }

    #[tokio::test]
    async fn fallback_catalog_localizes_descriptions_and_voice_errors_are_typed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(429).set_body_string("limited"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/voices"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        let catalog = local_tts(&server, Duration::from_secs(3))
            .catalog("es-MX")
            .await;
        assert_eq!(catalog.models.len(), 3);
        assert_eq!(catalog.models[0].description.as_deref(), Some("Rápido"));
        assert_eq!(
            catalog.error.as_ref().unwrap().kind,
            ProviderErrorKind::RateLimited
        );
        assert_eq!(catalog.error.as_ref().unwrap().status, Some(429));
        assert!(catalog.voices.is_empty());
    }

    #[tokio::test]
    async fn synthesis_maps_auth_malformed_response_and_sdk_timeout_without_retry() {
        let unauthorized = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice123/stream/with-timestamps"))
            .respond_with(ResponseTemplate::new(403).set_body_string("restricted"))
            .mount(&unauthorized)
            .await;
        let error = local_tts(&unauthorized, Duration::from_secs(2))
            .synthesize("Hi", "m", "voice123", 1.0, true, "mp3_44100_128")
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
        assert_eq!(error.status, Some(403));

        let malformed = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice123/stream/with-timestamps"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"audio_base64":"%%%","alignment":null,"normalized_alignment":null}"#,
            ))
            .mount(&malformed)
            .await;
        let error = local_tts(&malformed, Duration::from_secs(2))
            .synthesize("Hi", "m", "voice123", 1.0, true, "mp3_44100_128")
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::MalformedResponse);

        let slow = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice123/stream/with-timestamps"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(TIMESTAMPS)
                    .set_delay(Duration::from_millis(120)),
            )
            .mount(&slow)
            .await;
        let error = local_tts(&slow, Duration::from_millis(25))
            .synthesize("Hi", "m", "voice123", 1.0, true, "mp3_44100_128")
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Timeout);
        assert_eq!(slow.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelling_timestamp_synthesis_cancels_the_in_flight_sdk_future() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/text-to-speech/voice123/stream/with-timestamps"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(TIMESTAMPS)
                    .set_delay(Duration::from_millis(300)),
            )
            .mount(&server)
            .await;
        let tts = local_tts(&server, Duration::from_secs(2));
        let request = tokio::spawn(async move {
            tts.synthesize("Hi", "m", "voice123", 1.0, true, "mp3_44100_128")
                .await
        });
        for _ in 0..40 {
            if server
                .received_requests()
                .await
                .is_some_and(|requests| !requests.is_empty())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn pagination_stops_after_twenty_pages() {
        let server = MockServer::start().await;
        mount_models(&server).await;
        Mock::given(method("GET"))
            .and(path("/v2/voices"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"voices":[],"has_more":true,"total_count":999,"next_page_token":"next"}"#,
            ))
            .mount(&server)
            .await;
        let catalog = local_tts(&server, Duration::from_secs(3))
            .catalog("en")
            .await;
        assert!(catalog.error.is_none());
        let voice_requests = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/v2/voices")
            .count();
        assert_eq!(voice_requests, 20);
    }
}
