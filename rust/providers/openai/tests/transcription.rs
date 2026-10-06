use super::support::{
    body, local_transcriber, reply_json, JSON_TRANSCRIPTION, VERBOSE_TRANSCRIPTION,
};
use crate::providers::ProviderErrorKind;
use std::time::Duration;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

#[tokio::test]
async fn sends_wav_and_stage_options_through_async_openai_audio_sdk() {
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
async fn diarization_keeps_the_sdk_default_response_format() {
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
