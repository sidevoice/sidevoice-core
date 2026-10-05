//! Fixtures shared by the OpenAI tests: recorded payloads, mock replies and the local transcriber.
use crate::providers::openai::{client::build_client, OpenAiTranscriber};
use std::time::Duration;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

pub(super) const JSON_TRANSCRIPTION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/openai_transcription.json"
));
pub(super) const VERBOSE_TRANSCRIPTION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/openai_verbose_transcription.json"
));
pub(super) const MODELS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/openai_models.json"
));

pub(super) fn local_transcriber(
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

pub(super) async fn reply_json(server: &MockServer, path_value: &str, body: &'static str) {
    Mock::given(method("POST"))
        .and(path(path_value))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

pub(super) fn body(request: &Request) -> String {
    String::from_utf8_lossy(&request.body).into_owned()
}
