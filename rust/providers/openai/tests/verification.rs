use super::support::MODELS;
use crate::providers::openai::{client::build_client, verify_client};
use std::time::Duration;
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

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
