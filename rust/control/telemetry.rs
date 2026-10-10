//! OpenTelemetry for the room: one `voice.call` span per browser, one child span
//! and one histogram measurement per turn stage, a few counters, and a server span
//! per HTTP request.
//!
//! This is the only module that knows OpenTelemetry exists. Everything is opt-in:
//! with `OTEL_EXPORTER_OTLP_ENDPOINT` unset no provider, exporter thread, HTTP
//! client or span is ever created, and every hook is a check of an `Option`.
//!
//! Export uses the standard SDK (`opentelemetry_sdk`) and OTLP/HTTP protobuf
//! exporter (`opentelemetry-otlp`). Spans leave through
//! a batch processor (bounded queue of 2048, dropped when full, one export at a
//! time); metrics are aggregated in memory and exported periodically as
//! cumulative explicit-bucket histograms. Neither ever blocks a call path: a
//! recording is an in-memory update, and the exporters run on their own threads.
//!
//! Privacy is not configurable. `ATTRIBUTES` is the whole vocabulary a span or a
//! measurement may carry — ids, timings, states, engine names — and
//! `attributes()` drops anything else. No transcript, no audio, no message text
//! reaches a span, an attribute or an event, whatever a caller passes. HTTP
//! spans carry only the method, the matched route template and the status.

mod attributes;
mod http;
mod transport;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use opentelemetry::metrics::{Counter, Histogram, MeterProvider as _};
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{
    Span as _, SpanKind, TraceContextExt, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use serde_json::{Map, Value};
use url::Url;

use attributes::{attributes, MAX_VALUE};
pub use http::{http_route, http_span};
use transport::Collector;

pub const STAGES: [&str; 10] = [
    "endpoint_silence",
    "recognition",
    "request_to_transcript",
    "transcript_to_delivery",
    "delivery_to_read",
    "read_to_reply",
    "input_queued_to_reply",
    "reply_to_dispatch",
    "provider_synthesis",
    "audio_received_to_playback",
];

const DEFAULT_SERVICE_NAME: &str = "sidevoice-core";
/// Explicit bucket bounds of every stage histogram, in milliseconds. The SDK's
/// defaults, written down so the export is not left to a default.
pub const BUCKETS_MS: [f64; 15] = [
    0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0, 5000.0, 7500.0,
    10000.0,
];
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// A browser may not hold more turn contexts than the latency trace holds turns.
const MAX_TURNS: usize = 128;
/// Open `voice.call` spans; a call beyond this is still measured, without a call span.
const MAX_CALLS: usize = 256;

fn key_values(values: &Value) -> Vec<KeyValue> {
    let Value::Object(kept) = attributes(values) else {
        return Vec::new();
    };
    kept.into_iter()
        .filter_map(|(key, value)| {
            let value: opentelemetry::Value = match value {
                Value::String(text) => text.into(),
                Value::Bool(flag) => flag.into(),
                Value::Number(number) => match number.as_i64() {
                    Some(integer) => integer.into(),
                    None => number.as_f64()?.into(),
                },
                _ => return None,
            };
            Some(KeyValue::new(key, value))
        })
        .collect()
}

/// `base` overlaid with `extra`, both still raw: `attributes()` filters at the edge.
fn merged(base: &Map<String, Value>, extra: &Value) -> Value {
    let mut out = base.clone();
    if let Some(extra) = extra.as_object() {
        out.extend(
            extra
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    Value::Object(out)
}

/// A known stage with a finite duration of at most an hour.
fn admits(stage: &str, milliseconds: f64) -> bool {
    STAGES.contains(&stage)
        && milliseconds.is_finite()
        && (0.0..=3_600_000.0).contains(&milliseconds)
}

/// The counters the core keeps, by the name a collector sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counted {
    /// Playback stalls a browser reported.
    Stalls,
    /// Turns or playbacks the room cancelled.
    Cancels,
    /// Input receipts, by status.
    Receipts,
    /// Input deliveries retried.
    Redeliveries,
}
impl Counted {
    const ALL: [Counted; 4] = [
        Counted::Stalls,
        Counted::Cancels,
        Counted::Receipts,
        Counted::Redeliveries,
    ];
    fn name(self) -> &'static str {
        match self {
            Counted::Stalls => "sidevoice.audio.stalls",
            Counted::Cancels => "sidevoice.turn.cancels",
            Counted::Receipts => "sidevoice.input.receipts",
            Counted::Redeliveries => "sidevoice.input.redeliveries",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Counted::Stalls => "Playback stalls reported by a browser",
            Counted::Cancels => "Turns or playbacks the room cancelled",
            Counted::Receipts => "Input receipts, by status",
            Counted::Redeliveries => "Input deliveries retried",
        }
    }
}

/// One browser's call: its `voice.call` span, what the call is made of, and the
/// root spans the browser opened for its turns.
struct CallTrace {
    call: Context,
    facts: Map<String, Value>,
    turns: VecDeque<((String, u64), Context)>,
}

pub struct Telemetry {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    tracer: SdkTracer,
    histograms: Vec<(&'static str, Histogram<f64>)>,
    counters: Vec<(Counted, Counter<u64>)>,
    calls: Mutex<HashMap<String, CallTrace>>,
}

impl Telemetry {
    /// `None` is the entire disabled state. No exporter, HTTP client or thread is
    /// allocated until an explicit endpoint exists.
    pub fn configured(endpoint: Option<&str>) -> Option<Self> {
        Self::configure(endpoint, None)
    }

    /// The providers for `endpoint` (the OTLP base URL; `/v1/traces` and
    /// `/v1/metrics` are appended), named `service_name` or `sidevoice-core`.
    /// Needs a Tokio runtime, which carries the export requests.
    pub fn configure(endpoint: Option<&str>, service_name: Option<&str>) -> Option<Self> {
        let endpoint = endpoint?.trim();
        if endpoint.is_empty() {
            return None;
        }
        let mut base = Url::parse(endpoint).ok()?;
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return None;
        }
        base.set_query(None);
        base.set_fragment(None);
        let root = base.path().trim_end_matches('/').to_owned();
        let signal = |path: &str| {
            let mut url = base.clone();
            url.set_path(&format!("{root}{path}"));
            url.to_string()
        };
        let collector = Collector {
            client: reqwest::Client::builder()
                .timeout(EXPORT_TIMEOUT)
                .build()
                .ok()?,
            runtime: tokio::runtime::Handle::try_current().ok()?,
        };
        let service_name = service_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(DEFAULT_SERVICE_NAME)
            .to_owned();
        let resource = Resource::builder()
            .with_service_name(service_name)
            .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
            .build();
        let spans = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal("/v1/traces"))
            .with_timeout(EXPORT_TIMEOUT)
            .with_http_client(collector.clone())
            .build()
            .ok()?;
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal("/v1/metrics"))
            .with_timeout(EXPORT_TIMEOUT)
            .with_http_client(collector)
            .with_temporality(Temporality::Cumulative)
            .build()
            .ok()?;
        let tracer_provider = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(spans)
            .build();
        let meter_provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(PeriodicReader::builder(metrics).build())
            .build();
        let tracer = tracer_provider.tracer("sidevoice.room");
        let meter = meter_provider.meter("sidevoice.room");
        // One histogram per stage, named exactly like the span and the stats row.
        let histograms = STAGES
            .iter()
            .map(|&stage| {
                let histogram = meter
                    .f64_histogram(format!("sidevoice.turn.{stage}"))
                    .with_unit("ms")
                    .with_description(format!("Stage {stage} of one turn"))
                    .with_boundaries(BUCKETS_MS.to_vec())
                    .build();
                (stage, histogram)
            })
            .collect();
        let counters = Counted::ALL
            .iter()
            .map(|&counted| {
                let counter = meter
                    .u64_counter(counted.name())
                    .with_description(counted.description())
                    .build();
                (counted, counter)
            })
            .collect();
        Some(Self {
            tracer_provider,
            meter_provider,
            tracer,
            histograms,
            counters,
            calls: Mutex::new(HashMap::new()),
        })
    }

    /// The configuration from the environment: `OTEL_EXPORTER_OTLP_ENDPOINT` switches it on,
    /// `OTEL_SERVICE_NAME` names the service.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        Self::configure(
            var("OTEL_EXPORTER_OTLP_ENDPOINT").as_deref(),
            var("OTEL_SERVICE_NAME").as_deref(),
        )
    }

    pub fn from_env() -> Option<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Export what is buffered now. Blocks until the collector answered or timed out:
    /// call it from a blocking context, never from a call path.
    pub fn flush_blocking(&self) {
        let _ = self.tracer_provider.force_flush();
        let _ = self.meter_provider.force_flush();
    }

    // ----- the primitives every call site uses -----

    /// The remote span a W3C `traceparent` names, or `None` when there is none to continue.
    fn context_from(traceparent: Option<&str>) -> Option<Context> {
        let traceparent = traceparent.filter(|value| !value.is_empty())?;
        let carrier = HashMap::from([(
            "traceparent".to_owned(),
            traceparent.chars().take(MAX_VALUE).collect::<String>(),
        )]);
        let context = TraceContextPropagator::new().extract(&carrier);
        let valid = context.span().span_context().is_valid();
        valid.then_some(context)
    }

    fn common(&self, sid: &str, extra: &Value) -> Value {
        let calls = self.calls.lock().expect("telemetry lock");
        let mut base = calls
            .get(sid)
            .map(|call| call.facts.clone())
            .unwrap_or_default();
        base.insert("sidevoice.session_id".into(), Value::String(sid.to_owned()));
        merged(&base, extra)
    }

    /// The span a stage of this turn hangs from: the root the browser opened for the
    /// turn, else this call's `voice.call` span, else none (the stage is its own root).
    fn parent(&self, sid: &str, thread: &str, revision: u64) -> Context {
        let calls = self.calls.lock().expect("telemetry lock");
        let Some(call) = calls.get(sid) else {
            return Context::new();
        };
        call.turns
            .iter()
            .rev()
            .find(|((t, r), _)| t == thread && *r == revision)
            .map(|(_, context)| context.clone())
            .unwrap_or_else(|| call.call.clone())
    }

    // ----- the call this browser is in -----

    /// The hello's `traceparent`: everything this browser does is inside the browser's
    /// call span. What the call is made of goes on it once, and on every stage.
    pub fn call_started(&self, sid: &str, traceparent: Option<&str>, facts: &Value) {
        let Value::Object(facts) = attributes(facts) else {
            return;
        };
        let parent = Self::context_from(traceparent).unwrap_or_default();
        let mut values = facts.clone();
        values.insert("sidevoice.session_id".into(), Value::String(sid.to_owned()));
        let mut calls = self.calls.lock().expect("telemetry lock");
        if let Some(old) = calls.remove(sid) {
            old.call.span().end();
        }
        if calls.len() >= MAX_CALLS {
            return;
        }
        let span = self
            .tracer
            .span_builder("voice.call")
            .with_kind(SpanKind::Server)
            .with_attributes(key_values(&Value::Object(values)))
            .start_with_context(&self.tracer, &parent);
        calls.insert(
            sid.to_owned(),
            CallTrace {
                call: parent.with_span(span),
                facts,
                turns: VecDeque::new(),
            },
        );
    }

    pub fn call_ended(&self, sid: &str, reason: &str) {
        let Some(call) = self.calls.lock().expect("telemetry lock").remove(sid) else {
            return;
        };
        let span = call.call.span();
        span.set_attributes(key_values(&serde_json::json!({"sidevoice.reason": reason})));
        span.end();
    }

    /// The browser answered the turn-start event with the root span it opened for it.
    pub fn turn_context(&self, sid: &str, thread: &str, revision: u64, traceparent: Option<&str>) {
        let Some(context) = Self::context_from(traceparent) else {
            return;
        };
        let mut calls = self.calls.lock().expect("telemetry lock");
        let Some(call) = calls.get_mut(sid) else {
            return;
        };
        call.turns
            .retain(|((t, r), _)| !(t == thread && *r == revision));
        call.turns
            .push_back(((thread.to_owned(), revision), context));
        while call.turns.len() > MAX_TURNS {
            call.turns.pop_front();
        }
    }

    /// What the browser's output did, on the call span: one trace, not a second channel.
    pub fn audio_event(&self, sid: &str, kind: &str, values: &Value) {
        let event = format!("voice.audio.{}", kind.chars().take(40).collect::<String>());
        let common = self.common(sid, values);
        if let Some(call) = self.calls.lock().expect("telemetry lock").get(sid) {
            call.call.span().add_event(event, key_values(&common));
        }
        if kind == "stall" {
            self.count(Counted::Stalls, &self.common(sid, &Value::Null));
        }
    }

    // ----- one turn -----

    /// A stage measured between two marks of the room's monotonic clock, placed on
    /// the wall clock from `now_micros`, the same clock read at the time of the call.
    #[expect(
        clippy::too_many_arguments,
        reason = "a stage is named by its call, turn, stage and two marks"
    )]
    pub fn stage(
        &self,
        sid: &str,
        thread: &str,
        revision: u64,
        stage: &str,
        start_micros: u64,
        end_micros: u64,
        now_micros: u64,
        values: &Value,
    ) {
        let Some(elapsed) = end_micros.checked_sub(start_micros) else {
            return;
        };
        let end = SystemTime::now()
            .checked_sub(Duration::from_micros(now_micros.saturating_sub(end_micros)))
            .unwrap_or_else(SystemTime::now);
        self.emit(
            sid,
            thread,
            revision,
            stage,
            Duration::from_micros(elapsed),
            end,
            values,
        );
    }

    /// A stage somebody else measured (the browser's recognition request, a provider's
    /// HTTP request): its duration is the measurement, and it ends now.
    pub fn duration_stage(
        &self,
        sid: &str,
        thread: &str,
        revision: u64,
        stage: &str,
        milliseconds: f64,
        values: &Value,
    ) {
        if !admits(stage, milliseconds) {
            return;
        }
        self.emit(
            sid,
            thread,
            revision,
            stage,
            Duration::from_secs_f64(milliseconds / 1000.0),
            SystemTime::now(),
            values,
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a stage is named by its call, turn, stage and interval"
    )]
    fn emit(
        &self,
        sid: &str,
        thread: &str,
        revision: u64,
        stage: &str,
        elapsed: Duration,
        end: SystemTime,
        values: &Value,
    ) {
        let milliseconds = (elapsed.as_secs_f64() * 100_000.0).round() / 100.0;
        if !admits(stage, milliseconds) {
            return;
        }
        let common = self.common(
            sid,
            &merged(
                &Map::from_iter([
                    ("sidevoice.thread_id".into(), Value::String(thread.into())),
                    ("sidevoice.turn_revision".into(), Value::from(revision)),
                ]),
                values,
            ),
        );
        let start = end.checked_sub(elapsed).unwrap_or(end);
        let parent = self.parent(sid, thread, revision);
        let mut span_values = common.clone();
        span_values["sidevoice.stage"] = Value::String(stage.to_owned());
        let mut span = self
            .tracer
            .span_builder(stage.to_owned())
            .with_start_time(start)
            .with_attributes(key_values(&span_values))
            .start_with_context(&self.tracer, &parent);
        span.end_with_timestamp(end);
        self.record(stage, milliseconds, &common);
    }

    /// The histogram half of a stage, alone. For the browser's own playback stage,
    /// whose span is the browser's.
    pub fn observe(&self, sid: &str, stage: &str, milliseconds: f64, values: &Value) {
        if admits(stage, milliseconds) {
            self.record(stage, milliseconds, &self.common(sid, values));
        }
    }

    fn record(&self, stage: &str, milliseconds: f64, values: &Value) {
        if let Some((_, histogram)) = self.histograms.iter().find(|(name, _)| *name == stage) {
            histogram.record(milliseconds, &key_values(values));
        }
    }

    pub fn count(&self, counted: Counted, values: &Value) {
        if let Some((_, counter)) = self.counters.iter().find(|(name, _)| *name == counted) {
            counter.add(1, &key_values(values));
        }
    }

    /// A receipt the room sent this browser for one of its turns.
    pub fn receipt(&self, sid: &str, status: &str, thread: Option<&str>) {
        self.count(
            Counted::Receipts,
            &self.common(
                sid,
                &serde_json::json!({"sidevoice.status": status, "sidevoice.thread_id": thread}),
            ),
        );
    }

    /// Cancellations counted for a call, with the turn they cancelled.
    pub fn cancelled(&self, sid: &str, reason: &str, thread: Option<&str>, revision: Option<u64>) {
        self.count(
            Counted::Cancels,
            &self.common(
                sid,
                &serde_json::json!({"sidevoice.reason": reason, "sidevoice.thread_id": thread,
                    "sidevoice.turn_revision": revision}),
            ),
        );
    }
}

static SHARED: OnceLock<Option<Arc<Telemetry>>> = OnceLock::new();

/// The process's telemetry, configured from the environment on first use (inside
/// the runtime), or `None` for the whole life of a process started without an
/// endpoint.
pub fn shared() -> Option<Arc<Telemetry>> {
    SHARED
        .get_or_init(|| Telemetry::from_env().map(Arc::new))
        .clone()
}

/// End the calls still open and export what is buffered, bounded in time.
pub async fn shutdown() {
    let Some(telemetry) = SHARED.get().cloned().flatten() else {
        return;
    };
    let open: Vec<String> = telemetry
        .calls
        .lock()
        .expect("telemetry lock")
        .keys()
        .cloned()
        .collect();
    for sid in open {
        telemetry.call_ended(&sid, "stopped");
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || telemetry.flush_blocking()),
    )
    .await;
}
