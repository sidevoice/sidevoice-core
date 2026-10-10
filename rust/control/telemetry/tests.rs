use std::sync::Arc;

use axum::routing::get;
use axum::{middleware, Router};
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value, KeyValue as ProtoKeyValue};
use opentelemetry_proto::tonic::metrics::v1::{metric, Metric};
use opentelemetry_proto::tonic::trace::v1::Span as ProtoSpan;
use prost::Message;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{attributes, http_route, http_span, Counted, Telemetry, BUCKETS_MS};

const CALL_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const CALL_PARENT: &str = "00f067aa0ba902b7";
const TURN_TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
const TURN_PARENT: &str = "b7ad6b7169203331";

#[test]
fn disabled_exporter_has_no_instance_or_task() {
    assert!(Telemetry::configured(None).is_none());
    assert!(Telemetry::configured(Some(" ")).is_none());
    assert!(Telemetry::configured(Some("file:///tmp/collector")).is_none());
    assert!(Telemetry::from_vars(|_| None).is_none());
    assert!(
        Telemetry::from_vars(|name| (name == "OTEL_SERVICE_NAME").then_some("x".to_owned()))
            .is_none()
    );
}

#[test]
fn private_values_and_unlisted_attributes_never_pass() {
    let kept = attributes(
        &json!({"sidevoice.thread_id":"x".repeat(500),"sidevoice.duration_ms":23.5,
        "sidevoice.transcript":"private", "sidevoice.audio":"private", "authorization":"secret",
        "sidevoice.reason":null, "sidevoice.kind":{"nested":"private"}, "sidevoice.stalls":3}),
    );
    assert_eq!(kept["sidevoice.thread_id"].as_str().unwrap().len(), 200);
    assert_eq!(kept["sidevoice.duration_ms"], 23.5);
    assert_eq!(kept["sidevoice.stalls"], 3);
    assert_eq!(kept.as_object().unwrap().len(), 3);
    assert!(!kept.to_string().contains("private"));
    assert!(!kept.to_string().contains("secret"));
}

#[test]
fn the_allow_list_is_twenty_three_names() {
    assert_eq!(super::attributes::ALLOWED.len(), 23);
    let every: serde_json::Map<String, serde_json::Value> = super::attributes::ALLOWED
        .iter()
        .map(|name| ((*name).to_owned(), json!("v")))
        .collect();
    let kept = attributes(&serde_json::Value::Object(every));
    assert_eq!(kept.as_object().unwrap().len(), 23);
}

async fn collector() -> MockServer {
    let server = MockServer::start().await;
    for signal in ["/otel/v1/traces", "/otel/v1/metrics"] {
        Mock::given(method("POST"))
            .and(path(signal))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
    }
    server
}

async fn flushed(
    telemetry: &Arc<Telemetry>,
    server: &MockServer,
) -> (Vec<ProtoSpan>, Vec<Metric>, Vec<ProtoKeyValue>) {
    let flushing = telemetry.clone();
    tokio::task::spawn_blocking(move || flushing.flush_blocking())
        .await
        .unwrap();
    let mut spans = Vec::new();
    let mut metrics = Vec::new();
    let mut resource = Vec::new();
    for request in server.received_requests().await.unwrap() {
        assert_eq!(
            request.headers.get("content-type").unwrap(),
            "application/x-protobuf"
        );
        match request.url.path() {
            "/otel/v1/traces" => {
                let export = ExportTraceServiceRequest::decode(request.body.as_slice()).unwrap();
                for batch in export.resource_spans {
                    resource = batch.resource.unwrap().attributes;
                    for scope in batch.scope_spans {
                        spans.extend(scope.spans);
                    }
                }
            }
            "/otel/v1/metrics" => {
                let export = ExportMetricsServiceRequest::decode(request.body.as_slice()).unwrap();
                for batch in export.resource_metrics {
                    resource = batch.resource.unwrap().attributes;
                    for scope in batch.scope_metrics {
                        metrics.extend(scope.metrics);
                    }
                }
            }
            other => panic!("unexpected export to {other}"),
        }
    }
    (spans, metrics, resource)
}

fn text(attributes: &[ProtoKeyValue], key: &str) -> Option<String> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref()?.value.as_ref())
        .and_then(|value| match value {
            any_value::Value::StringValue(text) => Some(text.clone()),
            any_value::Value::IntValue(number) => Some(number.to_string()),
            _ => None,
        })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn stages_hang_from_the_call_and_export_as_cumulative_bucketed_histograms() {
    let server = collector().await;
    let telemetry = Arc::new(
        Telemetry::configure(
            Some(&format!("{}/otel/", server.uri())),
            Some("sidevoice-test"),
        )
        .unwrap(),
    );
    telemetry.call_started(
        "s1",
        Some(&format!("00-{CALL_TRACE}-{CALL_PARENT}-01")),
        &json!({"sidevoice.stt_place":"device","sidevoice.transcript":"private words"}),
    );
    telemetry.turn_context(
        "s1",
        "t1",
        2,
        Some(&format!("00-{TURN_TRACE}-{TURN_PARENT}-01")),
    );
    // Turn 2 has the browser's root span; turn 3 has none and hangs from voice.call.
    telemetry.stage(
        "s1",
        "t1",
        2,
        "recognition",
        1_000_000,
        1_250_000,
        2_000_000,
        &json!({"sidevoice.utterance_id":"u1","text":"private words"}),
    );
    telemetry.stage(
        "s1",
        "t1",
        2,
        "recognition",
        3_000_000,
        3_030_000,
        3_000_000,
        &json!({"sidevoice.utterance_id":"u1"}),
    );
    telemetry.duration_stage("s1", "t1", 3, "provider_synthesis", 812.5, &json!({}));
    telemetry.stage("s1", "t1", 2, "not_a_stage", 0, 1, 1, &json!({}));
    telemetry.observe("s1", "audio_received_to_playback", 4.0, &json!({}));
    telemetry.count(
        Counted::Redeliveries,
        &json!({"sidevoice.harness":"claude"}),
    );
    telemetry.count(
        Counted::Redeliveries,
        &json!({"sidevoice.harness":"claude"}),
    );
    telemetry.receipt("s1", "read", Some("t1"));
    telemetry.audio_event("s1", "stall", &json!({"sidevoice.stalls":1}));
    telemetry.call_ended("s1", "disconnected");

    let (spans, metrics, resource) = flushed(&telemetry, &server).await;
    assert_eq!(
        text(&resource, "service.name").as_deref(),
        Some("sidevoice-test")
    );
    assert_eq!(
        text(&resource, "service.version").as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    let call = spans.iter().find(|span| span.name == "voice.call").unwrap();
    assert_eq!(hex(&call.trace_id), CALL_TRACE);
    assert_eq!(hex(&call.parent_span_id), CALL_PARENT);
    assert_eq!(
        text(&call.attributes, "sidevoice.stt_place").as_deref(),
        Some("device")
    );
    assert_eq!(
        text(&call.attributes, "sidevoice.reason").as_deref(),
        Some("disconnected")
    );
    assert!(call
        .events
        .iter()
        .any(|event| event.name == "voice.audio.stall"));

    let recognitions: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "recognition")
        .collect();
    assert_eq!(recognitions.len(), 2);
    for span in &recognitions {
        assert_eq!(hex(&span.trace_id), TURN_TRACE);
        assert_eq!(hex(&span.parent_span_id), TURN_PARENT);
        assert_eq!(
            text(&span.attributes, "sidevoice.stage").as_deref(),
            Some("recognition")
        );
        assert_eq!(
            text(&span.attributes, "sidevoice.turn_revision").as_deref(),
            Some("2")
        );
        assert_eq!(
            text(&span.attributes, "sidevoice.stt_place").as_deref(),
            Some("device")
        );
    }
    let first = recognitions
        .iter()
        .find(|span| span.end_time_unix_nano - span.start_time_unix_nano == 250_000_000);
    assert!(first.is_some(), "the 250 ms stage keeps its length");
    let provider = spans
        .iter()
        .find(|span| span.name == "provider_synthesis")
        .unwrap();
    assert_eq!(hex(&provider.trace_id), CALL_TRACE);
    assert_eq!(provider.parent_span_id, call.span_id);
    assert!(!spans.iter().any(|span| span.name == "not_a_stage"));
    assert!(!spans
        .iter()
        .any(|span| span.name == "audio_received_to_playback"));

    let recognition = metrics
        .iter()
        .find(|metric| metric.name == "sidevoice.turn.recognition")
        .unwrap();
    assert_eq!(recognition.unit, "ms");
    let Some(metric::Data::Histogram(histogram)) = &recognition.data else {
        panic!("recognition is not a histogram");
    };
    assert_eq!(histogram.aggregation_temporality, 2, "cumulative");
    assert_eq!(histogram.data_points.len(), 1);
    let point = &histogram.data_points[0];
    assert_eq!(point.count, 2);
    assert!((point.sum.unwrap() - 280.0).abs() < 0.01);
    assert_eq!(point.explicit_bounds, BUCKETS_MS.to_vec());
    assert_eq!(point.bucket_counts.len(), BUCKETS_MS.len() + 1);
    // 30 ms lands in (25, 50], 250 ms in (100, 250].
    assert_eq!(point.bucket_counts[4], 1);
    assert_eq!(point.bucket_counts[7], 1);
    assert!(text(&point.attributes, "sidevoice.stage").is_none());
    assert_eq!(
        text(&point.attributes, "sidevoice.session_id").as_deref(),
        Some("s1")
    );
    assert!(metrics
        .iter()
        .any(|metric| metric.name == "sidevoice.turn.provider_synthesis"));
    assert!(metrics
        .iter()
        .any(|metric| metric.name == "sidevoice.turn.audio_received_to_playback"));

    let redeliveries = metrics
        .iter()
        .find(|metric| metric.name == "sidevoice.input.redeliveries")
        .unwrap();
    let Some(metric::Data::Sum(sum)) = &redeliveries.data else {
        panic!("redeliveries is not a sum");
    };
    assert!(sum.is_monotonic);
    assert_eq!(sum.aggregation_temporality, 2);
    for name in ["sidevoice.input.receipts", "sidevoice.audio.stalls"] {
        assert!(metrics.iter().any(|metric| metric.name == name), "{name}");
    }

    let everything = format!("{spans:?}{metrics:?}");
    assert!(!everything.contains("private"));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_requests_become_server_spans_named_by_route_template() {
    let server = collector().await;
    let telemetry =
        Arc::new(Telemetry::configure(Some(&format!("{}/otel", server.uri())), None).unwrap());
    let app = Router::new()
        .route("/api/items/{id}", get(|| async { "ok" }))
        .route_layer(middleware::from_fn(http_route))
        .layer(middleware::from_fn_with_state(telemetry.clone(), http_span));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::new();
    client
        .get(format!(
            "http://{address}/api/items/secret-id?token=private"
        ))
        .header("traceparent", format!("00-{CALL_TRACE}-{CALL_PARENT}-01"))
        .send()
        .await
        .unwrap();
    client
        .get(format!("http://{address}/nowhere"))
        .send()
        .await
        .unwrap();

    let (spans, _, resource) = flushed(&telemetry, &server).await;
    assert_eq!(
        text(&resource, "service.name").as_deref(),
        Some("sidevoice-core")
    );
    let item = spans
        .iter()
        .find(|span| span.name == "GET /api/items/{id}")
        .unwrap();
    assert_eq!(hex(&item.trace_id), CALL_TRACE);
    assert_eq!(hex(&item.parent_span_id), CALL_PARENT);
    assert_eq!(
        text(&item.attributes, "http.response.status_code").as_deref(),
        Some("200")
    );
    assert!(spans.iter().any(|span| span.name == "GET"));
    let everything = format!("{spans:?}");
    assert!(!everything.contains("secret-id"));
    assert!(!everything.contains("private"));
}
