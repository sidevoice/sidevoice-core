//! Reads a small request body before routing, so a refusal never closes on unread input.
//!
//! A route that answers without extracting the body (a 404, a guard refusal) leaves the client's bytes unread when
//! the connection closes. On Windows that close is a reset (`WSAECONNABORTED`/`WSAECONNRESET`) that discards the
//! response the client had not yet read; Unix mostly delivers it. Reading the body first makes every answer reach the
//! client on every OS.
//!
//! Limits, on purpose: only bodies declared by `Content-Length` up to [`DRAIN_LIMIT`] are read. Chunked bodies and
//! larger ones are left to their routes and can still be closed on unread, so a refusal of those may still reset on
//! Windows. The read has a deadline ([`DRAIN_DEADLINE`]): a peer that declares a body and withholds it is answered
//! 408 and closed, before any guard check runs. That is the one case where the guard does not decide; every request
//! whose body arrives in time reaches it unchanged. The server has no header-read timeout, so a peer that stalls
//! earlier than this keeps its connection as it did before; this layer adds no new way to hold one past the deadline.

use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::header::{CONNECTION, CONTENT_LENGTH};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

/// Larger bodies are left to the routes: every route that takes one applies its own limit.
const DRAIN_LIMIT: usize = 64 * 1024;
/// How long a declared body may take to arrive.
pub(super) const DRAIN_DEADLINE: Duration = Duration::from_secs(5);

pub(super) async fn drain(
    axum::extract::State(deadline): axum::extract::State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    let declared = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if !declared.is_some_and(|length| length > 0 && length <= DRAIN_LIMIT) {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    match tokio::time::timeout(deadline, to_bytes(body, DRAIN_LIMIT)).await {
        Ok(Ok(bytes)) => {
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        // Stalled or broken mid-body: refuse and close rather than hold the connection.
        _ => Response::builder()
            .status(StatusCode::REQUEST_TIMEOUT)
            .header(CONNECTION, "close")
            .body(Body::empty())
            .expect("a static response"),
    }
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
            .layer(middleware::from_fn_with_state(DRAIN_DEADLINE, drain))
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

#[cfg(test)]
mod tcp_tests {
    use std::time::Duration;

    use axum::http::StatusCode;
    use axum::middleware;
    use axum::routing::post;
    use axum::Router;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::drain;

    async fn serve(deadline: Duration) -> std::net::SocketAddr {
        let app = Router::new()
            .route("/refused", post(|| async { StatusCode::NOT_FOUND }))
            .layer(middleware::from_fn_with_state(deadline, drain));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        address
    }

    const HEAD: &str =
        "POST /refused HTTP/1.1\r\nhost: x\r\nconnection: close\r\ncontent-length: 2\r\n\r\n";

    #[tokio::test]
    async fn a_refusal_over_tcp_reaches_a_client_that_sends_its_body_after_the_head() {
        let address = serve(Duration::from_secs(5)).await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(HEAD.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        stream.write_all(b"{}").await.unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 404"));
    }

    #[tokio::test]
    async fn a_withheld_body_does_not_hold_the_connection_past_the_deadline() {
        let address = serve(Duration::from_millis(200)).await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(HEAD.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut raw))
            .await
            .expect("the server closed the connection")
            .unwrap();
        assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 408"));
    }
}
