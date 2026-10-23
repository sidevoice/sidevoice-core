//! Reads a small request body before routing, so a refusal never closes on unread input.
//!
//! A route that answers without extracting the body (a 404, a guard refusal) leaves the client's bytes unread when
//! the connection closes. On Windows that close is a reset (`WSAECONNABORTED`/`WSAECONNRESET`) that discards the
//! response the client had not yet read; Unix mostly delivers it. Reading the body first makes every answer reach the
//! client on every OS.

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::header::CONTENT_LENGTH;
use axum::middleware::Next;
use axum::response::Response;

/// Larger bodies are left to the routes: every route that takes one applies its own limit.
const DRAIN_LIMIT: usize = 64 * 1024;

pub(super) async fn drain(request: Request, next: Next) -> Response {
    let declared = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if !declared.is_some_and(|length| length > 0 && length <= DRAIN_LIMIT) {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, DRAIN_LIMIT).await else {
        // The client went away mid-body: there is nobody to answer.
        return Response::new(Body::empty());
    };
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use axum::http::StatusCode;
    use axum::middleware;
    use axum::routing::post;
    use axum::Router;
    use futures_util::stream;
    use tower::ServiceExt;

    use super::*;

    fn counted(finished: &Arc<AtomicBool>, declared: usize) -> Request {
        let finished = finished.clone();
        let chunks = stream::iter([Ok::<_, std::io::Error>(vec![b'x'; declared])]);
        let body = Body::from_stream(futures_util::StreamExt::chain(
            chunks,
            stream::poll_fn(move |_| {
                finished.store(true, Ordering::SeqCst);
                std::task::Poll::Ready(None)
            }),
        ));
        Request::post("/refused")
            .header(CONTENT_LENGTH, declared)
            .body(body)
            .unwrap()
    }

    fn app() -> Router {
        Router::new()
            .route("/refused", post(|| async { StatusCode::NOT_FOUND }))
            .layer(middleware::from_fn(drain))
    }

    #[tokio::test]
    async fn a_refusal_that_ignores_the_body_has_read_it_by_the_time_it_answers() {
        let finished = Arc::new(AtomicBool::new(false));
        let answer = app().oneshot(counted(&finished, 2)).await.unwrap();
        assert_eq!(answer.status(), StatusCode::NOT_FOUND);
        assert!(finished.load(Ordering::SeqCst), "the body was left unread");
    }

    #[tokio::test]
    async fn a_body_past_the_limit_is_left_to_its_route() {
        let finished = Arc::new(AtomicBool::new(false));
        let answer = app()
            .oneshot(counted(&finished, DRAIN_LIMIT + 1))
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::NOT_FOUND);
        assert!(!finished.load(Ordering::SeqCst));
    }
}
