//! The retry policy of the official OpenAI SDKs: retryable statuses, server hints and backoff.

use async_openai::{error::OpenAIError, middleware::HttpRequestFactory};
use rand::Rng;
use reqwest::Response;
use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime},
};
use tower::retry::Policy;

const INITIAL_RETRY_DELAY: f64 = 0.5;
const MAX_RETRY_DELAY: f64 = 8.0;
const MAX_RETRY_AFTER: f64 = 120.0;

#[derive(Clone)]
pub(super) struct RetryPolicy {
    max_retries: usize,
    attempts: usize,
}

impl RetryPolicy {
    pub(super) fn new(max_retries: usize) -> Self {
        Self {
            max_retries,
            attempts: 0,
        }
    }
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
