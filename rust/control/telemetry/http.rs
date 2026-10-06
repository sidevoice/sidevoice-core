//! Server spans for HTTP requests: method, matched route template and status only.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::trace::{Span as _, SpanKind, Status, Tracer as _};
use opentelemetry::KeyValue;

use super::Telemetry;

/// The route template an inner layer found, handed back to the outer span.
#[derive(Clone, Default)]
struct MatchedRoute(Arc<Mutex<Option<String>>>);

/// Inner half of the HTTP spans (a `route_layer`): note the matched route template,
/// never the concrete path, so ids in a path never reach a span.
pub async fn http_route(request: Request, next: Next) -> Response {
    if let (Some(slot), Some(path)) = (
        request.extensions().get::<MatchedRoute>(),
        request.extensions().get::<MatchedPath>(),
    ) {
        *slot.0.lock().expect("route lock") = Some(path.as_str().to_owned());
    }
    next.run(request).await
}

/// Outer half of the HTTP spans: one server span per request, continuing the
/// caller's `traceparent`, with the method, the route template and the status.
pub async fn http_span(
    State(telemetry): State<Arc<Telemetry>>,
    mut request: Request,
    next: Next,
) -> Response {
    let parent = Telemetry::context_from(
        request
            .headers()
            .get("traceparent")
            .and_then(|value| value.to_str().ok()),
    )
    .unwrap_or_default();
    let method = request.method().as_str().to_owned();
    let route = MatchedRoute::default();
    request.extensions_mut().insert(route.clone());
    let start = SystemTime::now();
    let response = next.run(request).await;
    let route = route.0.lock().expect("route lock").take();
    let status = response.status().as_u16();
    let mut attributes = vec![
        KeyValue::new("http.request.method", method.clone()),
        KeyValue::new("http.response.status_code", i64::from(status)),
    ];
    if let Some(route) = &route {
        attributes.push(KeyValue::new("http.route", route.clone()));
    }
    let name = match route {
        Some(route) => format!("{method} {route}"),
        None => method,
    };
    let mut span = telemetry
        .tracer
        .span_builder(name)
        .with_kind(SpanKind::Server)
        .with_start_time(start)
        .with_attributes(attributes)
        .start_with_context(&telemetry.tracer, &parent);
    if status >= 500 {
        span.set_status(Status::error(""));
    }
    span.end();
    response
}
