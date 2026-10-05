//! Construction of the async-openai client over a timed reqwest client and the retry middleware.

use super::retry::RetryPolicy;
use crate::providers::{ProviderError, ProviderErrorKind};
use async_openai::{config::OpenAIConfig, middleware::ReqwestService, Client};
use reqwest::header::HeaderValue;
use std::time::Duration;
use tower::ServiceBuilder;

pub(super) fn build_client(
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
        .map_err(|error| ProviderError::transport(error.is_timeout(), None))?;
    let config = OpenAIConfig::new()
        .with_api_base(api_base)
        .with_api_key(api_key);
    let service = ServiceBuilder::new()
        .retry(RetryPolicy::new(max_retries))
        .service(ReqwestService::new(http.clone()));
    Ok(Client::with_config(config)
        .with_http_client(http)
        .with_http_service(service))
}
