//! Opt-in OTLP stage export from Room-owned immutable latency observations.
//! An unset endpoint constructs no client and cannot start an export task.

mod attributes;
mod otlp;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Semaphore;
use url::Url;

use attributes::attributes;

const STAGES: [&str; 10] = [
    "endpoint_silence",
    "recognition",
    "request_to_transcript",
    "transcript_to_delivery",
    "delivery_to_read",
    "read_to_reply",
    "input_queued_to_reply",
    "reply_to_synthesis",
    "provider_synthesis",
    "audio_received_to_playback",
];
const MAX_PENDING_EXPORTS: usize = 16;

pub struct Telemetry {
    endpoint: Url,
    client: reqwest::Client,
    pending: Arc<Semaphore>,
}

impl Telemetry {
    /// `None` is the entire disabled state. No exporter, HTTP client or task is
    /// allocated until an explicit endpoint exists.
    pub fn configured(endpoint: Option<&str>) -> Option<Self> {
        let endpoint = endpoint?.trim();
        if endpoint.is_empty() {
            return None;
        }
        let mut endpoint = Url::parse(endpoint).ok()?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return None;
        }
        let path = format!("{}/v1/metrics", endpoint.path().trim_end_matches('/'));
        endpoint.set_path(&path);
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        Some(Self {
            endpoint,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .ok()?,
            pending: Arc::new(Semaphore::new(MAX_PENDING_EXPORTS)),
        })
    }

    pub fn from_env() -> Option<Self> {
        Self::configured(std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok().as_deref())
    }

    /// Submit from a call path without waiting for the collector. Saturation
    /// drops diagnostics rather than delaying the call.
    pub fn try_observe(self: &Arc<Self>, stage: &str, milliseconds: f64, values: &Value) {
        if !admits(stage, milliseconds) {
            return;
        }
        let Ok(permit) = self.pending.clone().try_acquire_owned() else {
            return;
        };
        let exporter = self.clone();
        let stage = stage.to_owned();
        let values = attributes(values);
        tokio::spawn(async move {
            let _permit = permit;
            let _ = exporter.observe(&stage, milliseconds, &values).await;
        });
    }

    /// Export one finished stage as an OTLP histogram data point. Caller passes
    /// only already-admitted observations; this type owns no trace or revision.
    pub async fn observe(
        &self,
        stage: &str,
        milliseconds: f64,
        values: &Value,
    ) -> Result<(), reqwest::Error> {
        if !admits(stage, milliseconds) {
            return Ok(());
        }
        let mut values = attributes(values);
        values["sidevoice.stage"] = json!(stage);
        let payload = otlp::histogram(stage, milliseconds, &values);
        self.client
            .post(self.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload.to_string())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// A known stage with a finite duration of at most an hour.
fn admits(stage: &str, milliseconds: f64) -> bool {
    STAGES.contains(&stage)
        && milliseconds.is_finite()
        && (0.0..=3_600_000.0).contains(&milliseconds)
}
