use super::support::{
    assert_audio, delayed_body_server, local_tts, split_timestamp_server, TIMESTAMPS,
    TIMESTAMP_CHUNKS,
};
use crate::providers::elevenlabs::ElevenLabsTts;
use crate::providers::ProviderErrorKind;
use serde_json::Value;
use std::time::Duration;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

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
    let tts = ElevenLabsTts::with_config("test-key", &base_url, Duration::from_secs(3)).unwrap();
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
    assert!(request_headers.starts_with("POST /v1/text-to-speech/voice123/stream/with-timestamps?"));
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
async fn body_stream_timeouts_keep_timeout_kind_for_audio_and_timestamp_streams() {
    for timestamped in [false, true] {
        let (base_url, server) = delayed_body_server(timestamped).await;
        let tts =
            ElevenLabsTts::with_config("test-key", &base_url, Duration::from_millis(300)).unwrap();
        let error = tts
            .synthesize(
                "Hi",
                "eleven_multilingual_v2",
                "voice123",
                1.0,
                timestamped,
                "mp3_44100_128",
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Timeout);

        let request = String::from_utf8_lossy(&server.await.unwrap()).into_owned();
        let expected_path = if timestamped {
            "/v1/text-to-speech/voice123/stream/with-timestamps?"
        } else {
            "/v1/text-to-speech/voice123/stream?"
        };
        assert!(request.starts_with(&format!("POST {expected_path}")));
    }
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
