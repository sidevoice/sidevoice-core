use super::{ProviderError, ProviderErrorKind};
use async_openai::{
    config::OpenAIConfig,
    error::OpenAIError,
    middleware::{HttpRequestFactory, ReqwestService},
    types::audio::{
        AudioInput, AudioResponseFormat, CreateTranscriptionRequest, TranscriptionInclude,
    },
    Client,
};
use rand::Rng;
use reqwest::{header::HeaderValue, Response};
use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime},
};
use tower::{retry::Policy, ServiceBuilder};

const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const PYTHON_MAX_RETRIES: usize = 2;
const INITIAL_RETRY_DELAY: f64 = 0.5;
const MAX_RETRY_DELAY: f64 = 8.0;
const MAX_RETRY_AFTER: f64 = 120.0;

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
                PYTHON_MAX_RETRIES,
            )?,
        })
    }

    pub async fn catalog(&self) -> Result<Vec<String>, ProviderError> {
        let response = self.client.models().list().await.map_err(map_openai_error)?;
        let mut models: Vec<String> = response
            .data
            .into_iter()
            .map(|model| model.id)
            .filter(|id| {
                (id == "whisper-1" || (id.contains("transcribe")
                    && !id.contains("realtime")
                    && !id.contains("live")))
                    && id.len() <= 120
                    && id
                        .chars()
                        .next()
                        .is_some_and(|first| first.is_ascii_alphanumeric())
                    && id
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "._:-".contains(ch))
            })
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
        let response_format = if model.starts_with("whisper") {
            Some(AudioResponseFormat::VerboseJson)
        } else if model.contains("diarize") {
            None
        } else {
            Some(AudioResponseFormat::Json)
        };
        let include = (response_format == Some(AudioResponseFormat::Json))
            .then_some(vec![TranscriptionInclude::Logprobs]);
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
        // Parse only response fields Python consumes so null token logprobs remain compatible.
        let body = self
            .client
            .audio()
            .transcription()
            .create_raw(request)
            .await
            .map_err(map_openai_error)?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| ProviderError::new(ProviderErrorKind::MalformedResponse, None))?;
        let text = value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let probabilities: Vec<f64> = value
            .get("logprobs")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("logprob").and_then(serde_json::Value::as_f64))
            .collect();
        let mean_logprob = (!probabilities.is_empty())
            .then(|| probabilities.iter().sum::<f64>() / probabilities.len() as f64);
        Ok(Transcription { text, mean_logprob })
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

fn build_client(
    api_key: &str,
    api_base: &str,
    timeout: Duration,
    connect_timeout: Duration,
    max_retries: usize,
) -> Result<Client<OpenAIConfig>, ProviderError> {
    if api_key.is_empty() || HeaderValue::from_str(&format!("Bearer {api_key}")).is_err() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidConfiguration,
            None,
        ));
    }
    let http = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|error| {
            ProviderError::new(
                if error.is_timeout() {
                    ProviderErrorKind::Timeout
                } else {
                    ProviderErrorKind::Transport
                },
                None,
            )
        })?;
    let config = OpenAIConfig::new()
        .with_api_base(api_base)
        .with_api_key(api_key);
    let retry = RetryPolicy {
        max_retries,
        attempts: 0,
    };
    let service = ServiceBuilder::new()
        .retry(retry)
        .service(ReqwestService::new(http.clone()));
    Ok(Client::with_config(config)
        .with_http_client(http)
        .with_http_service(service))
}

fn map_openai_error(error: OpenAIError) -> ProviderError {
    match error {
        OpenAIError::ApiError(response) => {
            ProviderError::from_status(response.status_code.as_u16())
        }
        OpenAIError::Reqwest(error) => ProviderError::new(
            if error.is_timeout() {
                ProviderErrorKind::Timeout
            } else {
                ProviderErrorKind::Transport
            },
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

#[derive(Clone)]
struct RetryPolicy {
    max_retries: usize,
    attempts: usize,
}

impl Policy<HttpRequestFactory, Response, OpenAIError> for RetryPolicy {
    type Future = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

    fn retry(
        &mut self,
        _request: &mut HttpRequestFactory,
        result: &mut Result<Response, OpenAIError>,
    ) -> Option<Self::Future> {
        if self.attempts >= self.max_retries || !should_retry(result) {
            return None;
        }
        let delay = retry_delay(self.attempts, result);
        self.attempts += 1;
        Some(Box::pin(async move {
            tokio::time::sleep(delay).await;
        }))
    }

    fn clone_request(&mut self, request: &HttpRequestFactory) -> Option<HttpRequestFactory> {
        Some(request.clone())
    }
}

fn should_retry(result: &Result<Response, OpenAIError>) -> bool {
    let Some(response) = result.as_ref().ok() else {
        return matches!(
            result,
            Err(OpenAIError::Reqwest(error)) if error.is_connect() || error.is_timeout()
        );
    };
    if retry_after(response).is_some_and(|delay| delay.is_finite() && delay > MAX_RETRY_AFTER) {
        return false;
    }
    match response
        .headers()
        .get("x-should-retry")
        .and_then(|value| value.to_str().ok())
    {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    matches!(response.status().as_u16(), 408 | 409 | 429) || response.status().as_u16() >= 500
}

fn retry_delay(attempt: usize, result: &Result<Response, OpenAIError>) -> Duration {
    if let Some(delay) = result
        .as_ref()
        .ok()
        .and_then(retry_after)
        .filter(|delay| delay.is_finite() && *delay > 0.0 && *delay <= MAX_RETRY_AFTER)
    {
        return Duration::from_secs_f64(delay);
    }
    let base = (INITIAL_RETRY_DELAY * 2f64.powi(attempt as i32)).min(MAX_RETRY_DELAY);
    let jitter = 1.0 - 0.25 * rand::thread_rng().gen::<f64>();
    Duration::from_secs_f64(base * jitter)
}

fn retry_after(response: &Response) -> Option<f64> {
    if let Some(value) = response
        .headers()
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
    {
        return Some(value / 1000.0);
    }
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    let value = value.to_str().ok()?;
    if let Ok(seconds) = value.parse::<f64>() {
        return Some(seconds);
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(
        date.duration_since(SystemTime::now())
            .unwrap_or_default()
            .as_secs_f64(),
    )
}

#[cfg(test)]
mod tests {
    use super::{build_client, verify_client, OpenAiTranscriber};
    use crate::providers::ProviderErrorKind;
    use std::time::Duration;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, Request, ResponseTemplate,
    };

    const JSON_TRANSCRIPTION: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/openai_transcription.json"
    ));
    const VERBOSE_TRANSCRIPTION: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/openai_verbose_transcription.json"
    ));
    const MODELS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/rust_t4/openai_models.json"
    ));

    fn local_transcriber(
        server: &MockServer,
        timeout: Duration,
        retries: usize,
    ) -> OpenAiTranscriber {
        OpenAiTranscriber {
            client: build_client(
                "test-key",
                &format!("{}/v1", server.uri()),
                timeout,
                Duration::from_secs(1),
                retries,
            )
            .unwrap(),
        }
    }

    async fn reply_json(server: &MockServer, path_value: &str, body: &'static str) {
        Mock::given(method("POST"))
            .and(path(path_value))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    fn body(request: &Request) -> String {
        String::from_utf8_lossy(&request.body).into_owned()
    }

    #[tokio::test]
    async fn sends_wav_and_python_options_through_async_openai_audio_sdk() {
        let server = MockServer::start().await;
        reply_json(&server, "/v1/audio/transcriptions", JSON_TRANSCRIPTION).await;
        let transcriber = local_transcriber(&server, Duration::from_secs(5), 0);
        let result = transcriber
            .transcribe(
                b"RIFFWAVE-local-fixture",
                "gpt-4o-transcribe",
                Some("es"),
                Some("contexto"),
            )
            .await
            .unwrap();
        assert_eq!(result.text, "Hola mundo");
        assert_eq!(result.mean_logprob, Some(-0.5));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.headers["authorization"], "Bearer test-key");
        let multipart = body(request);
        for expected in [
            "name=\"file\"; filename=\"audio.wav\"\r\n\r\nRIFFWAVE-local-fixture",
            "name=\"model\"\r\n\r\ngpt-4o-transcribe",
            "name=\"language\"\r\n\r\nes",
            "name=\"prompt\"\r\n\r\ncontexto",
            "name=\"response_format\"\r\n\r\njson",
            "name=\"include[]\"\r\n\r\nlogprobs",
        ] {
            assert!(
                multipart.contains(expected),
                "multipart did not contain {expected:?}"
            );
        }
        assert!(!multipart.contains("Content-Type: audio/wav"));
    }

    #[tokio::test]
    async fn whisper_uses_verbose_json_without_logprobs_and_transcription_is_trimmed() {
        let server = MockServer::start().await;
        reply_json(&server, "/v1/audio/transcriptions", VERBOSE_TRANSCRIPTION).await;
        let result = local_transcriber(&server, Duration::from_secs(5), 0)
            .transcribe(b"RIFFWAVE", "whisper-1", None, None)
            .await
            .unwrap();
        assert_eq!(result.text, "Hello there");
        assert_eq!(result.mean_logprob, None);
        let requests = server.received_requests().await.unwrap();
        let multipart = body(&requests[0]);
        assert!(multipart.contains("name=\"response_format\"\r\n\r\nverbose_json"));
        assert!(!multipart.contains("include[]"));
    }

    #[tokio::test]
    async fn diarization_keeps_the_python_sdk_default_response_format() {
        let server = MockServer::start().await;
        reply_json(
            &server,
            "/v1/audio/transcriptions",
            r#"{"text":"Speaker A: hi","segments":[],"usage":{"type":"tokens","input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
        )
        .await;
        let result = local_transcriber(&server, Duration::from_secs(5), 0)
            .transcribe(b"RIFFWAVE", "gpt-4o-transcribe-diarize", Some("en"), None)
            .await
            .unwrap();
        assert_eq!(result.text, "Speaker A: hi");
        assert_eq!(result.mean_logprob, None);
        let requests = server.received_requests().await.unwrap();
        let multipart = body(&requests[0]);
        assert!(!multipart.contains("response_format"));
        assert!(!multipart.contains("include[]"));
    }

    #[tokio::test]
    async fn retries_rate_limit_once_then_returns_the_real_sdk_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after-ms", "1")
                    .set_body_string("{\"error\":{\"message\":\"rate limited\"}}"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        reply_json(&server, "/v1/audio/transcriptions", JSON_TRANSCRIPTION).await;
        let result = local_transcriber(&server, Duration::from_secs(5), 2)
            .transcribe(b"RIFFWAVE", "gpt-4o-transcribe", None, None)
            .await
            .unwrap();
        assert_eq!(result.text, "Hola mundo");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn status_malformed_body_and_timeout_map_to_stable_errors() {
        let unauthorized = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("{\"error\":{\"message\":\"secret diagnostic\"}}"),
            )
            .mount(&unauthorized)
            .await;
        let error = local_transcriber(&unauthorized, Duration::from_secs(5), 0)
            .transcribe(b"RIFFWAVE", "whisper-1", None, None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Unauthorized);
        assert_eq!(error.status, Some(401));
        assert!(!error.to_string().contains("secret diagnostic"));

        let malformed = MockServer::start().await;
        reply_json(&malformed, "/v1/audio/transcriptions", "not-json").await;
        let error = local_transcriber(&malformed, Duration::from_secs(5), 0)
            .transcribe(b"RIFFWAVE", "whisper-1", None, None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::MalformedResponse);

        let slow = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(VERBOSE_TRANSCRIPTION)
                    .set_delay(Duration::from_millis(120)),
            )
            .mount(&slow)
            .await;
        let error = local_transcriber(&slow, Duration::from_millis(25), 0)
            .transcribe(b"RIFFWAVE", "whisper-1", None, None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Timeout);
    }

    #[tokio::test]
    async fn key_verification_uses_the_models_endpoint_and_no_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MODELS))
            .mount(&server)
            .await;
        let client = build_client(
            "test-key",
            &format!("{}/v1", server.uri()),
            Duration::from_secs(2),
            Duration::from_secs(1),
            0,
        )
        .unwrap();
        verify_client(&client).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/v1/models");
    }

    #[tokio::test]
    async fn cancelling_a_transcription_waiter_drops_the_sdk_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(VERBOSE_TRANSCRIPTION)
                    .set_delay(Duration::from_millis(300)),
            )
            .mount(&server)
            .await;
        let transcriber = local_transcriber(&server, Duration::from_secs(2), 0);
        let request = tokio::spawn(async move {
            transcriber
                .transcribe(b"RIFFWAVE", "whisper-1", None, None)
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
}
