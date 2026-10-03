//! Opt-in OTLP stage export from Room-owned immutable latency observations.
//! An unset endpoint constructs no client and cannot start an export task.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use url::Url;

pub const STAGES: [&str; 10] = [
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

const ATTRIBUTES: [&str; 24] = [
    "sidevoice.session_id",
    "sidevoice.thread_id",
    "sidevoice.turn_revision",
    "sidevoice.reply_revision",
    "sidevoice.utterance_id",
    "sidevoice.status",
    "sidevoice.reason",
    "sidevoice.outcome",
    "sidevoice.kind",
    "sidevoice.stt_place",
    "sidevoice.stt_model",
    "sidevoice.stt_accelerator",
    "sidevoice.tts_provider",
    "sidevoice.tts_model",
    "sidevoice.turn_end_mode",
    "sidevoice.harness",
    "sidevoice.shared_audio",
    "sidevoice.synthesis_attempt",
    "sidevoice.stage",
    "sidevoice.duration_ms",
    "sidevoice.audio_output",
    "sidevoice.audio_context",
    "sidevoice.stalls",
    "sidevoice.build_id",
];

/// Exact Python allowlist, with strings bounded before they reach a collector.
pub fn attributes(values: &Value) -> Value {
    let mut kept = Map::new();
    if let Some(values) = values.as_object() {
        for (key, value) in values {
            if !ATTRIBUTES.contains(&key.as_str()) || value.is_null() {
                continue;
            }
            let value = match value {
                Value::String(text) => Value::String(text.chars().take(200).collect()),
                Value::Bool(_) | Value::Number(_) => value.clone(),
                _ => continue,
            };
            kept.insert(key.clone(), value);
        }
    }
    Value::Object(kept)
}

pub struct Telemetry {
    endpoint: Url,
    client: reqwest::Client,
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
            client: reqwest::Client::new(),
        })
    }

    pub fn from_env() -> Option<Self> {
        Self::configured(std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok().as_deref())
    }

    /// Export one finished stage as an OTLP histogram data point. Caller passes
    /// only already-admitted observations; this type owns no trace or revision.
    pub async fn observe(
        &self,
        stage: &str,
        milliseconds: f64,
        values: &Value,
    ) -> Result<(), reqwest::Error> {
        if !STAGES.contains(&stage)
            || !milliseconds.is_finite()
            || !(0.0..=3_600_000.0).contains(&milliseconds)
        {
            return Ok(());
        }
        let mut values = attributes(values);
        values["sidevoice.stage"] = json!(stage);
        let attrs = values
            .as_object()
            .expect("attributes object")
            .iter()
            .map(|(key, value)| {
                let wrapped = match value {
                    Value::String(text) => json!({"stringValue":text}),
                    Value::Bool(value) => json!({"boolValue":value}),
                    Value::Number(value) => {
                        json!({"doubleValue":value.as_f64().unwrap_or_default()})
                    }
                    _ => Value::Null,
                };
                json!({"key":key,"value":wrapped})
            })
            .collect::<Vec<_>>();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string();
        let payload = json!({"resourceMetrics":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"sidevoice-core"}}]},
            "scopeMetrics":[{"scope":{"name":"sidevoice.room"},"metrics":[{"name":format!("sidevoice.turn.{stage}"),"unit":"ms",
                "histogram":{"aggregationTemporality":2,"dataPoints":[{"attributes":attrs,"timeUnixNano":now,
                    "count":"1","sum":milliseconds,"bucketCounts":["1"]}]}}]}]}]});
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_exporter_has_no_instance_or_task() {
        assert!(Telemetry::configured(None).is_none());
        assert!(Telemetry::configured(Some(" ")).is_none());
        assert!(Telemetry::configured(Some("file:///tmp/collector")).is_none());
    }

    #[test]
    fn private_values_and_unlisted_attributes_never_pass() {
        let kept = attributes(
            &json!({"sidevoice.thread_id":"x".repeat(500),"sidevoice.duration_ms":23.5,
            "sidevoice.transcript":"private", "sidevoice.audio":"private", "authorization":"secret",
            "sidevoice.reason":null}),
        );
        assert_eq!(kept["sidevoice.thread_id"].as_str().unwrap().len(), 200);
        assert_eq!(kept["sidevoice.duration_ms"], 23.5);
        assert!(!kept.to_string().contains("private"));
        assert!(!kept.to_string().contains("secret"));
    }
}
