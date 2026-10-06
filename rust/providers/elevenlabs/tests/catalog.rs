use super::support::{
    local_tts, mount_models, LEGACY_VOICES, SPARSE_MODELS, SPARSE_VOICES, VOICES_ONE, VOICES_TWO,
};
use crate::providers::elevenlabs::{client, verify_client};
use crate::providers::ProviderErrorKind;
use std::time::Duration;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

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
async fn sparse_catalog_uses_sdk_defaults_and_voice_primary_language_precedence() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("xi-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SPARSE_MODELS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/voices"))
        .and(header("xi-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SPARSE_VOICES))
        .mount(&server)
        .await;

    let client = client("test-key", &server.uri(), Duration::from_secs(3)).unwrap();
    verify_client(&client).await.unwrap();
    let catalog = local_tts(&server, Duration::from_secs(3))
        .catalog("en")
        .await;

    assert_eq!(catalog.error, None);
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].id, "sparse-model");
    assert_eq!(catalog.models[0].label, "sparse-model");
    assert_eq!(catalog.models[0].description, None);
    assert_eq!(catalog.voices.len(), 1);
    assert_eq!(catalog.voices[0].languages, ["es"]);
    assert_eq!(
        catalog.voices[0].description.as_deref(),
        Some("legacy-category")
    );

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v1/models")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v2/voices")
            .count(),
        2
    );
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
    let client = client("test-key", &server.uri(), Duration::from_secs(3)).unwrap();
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
