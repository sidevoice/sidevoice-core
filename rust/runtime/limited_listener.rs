//! Cap live TCP connections, including upgraded WebSockets and idle keep-alive sockets.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) const MAX_CONNECTIONS: usize = 256;

pub(super) struct LimitedListener {
    tcp: TcpListener,
    permits: Arc<Semaphore>,
    logged: Option<Instant>,
}

impl LimitedListener {
    pub(super) fn new(tcp: TcpListener) -> Self {
        Self {
            tcp,
            permits: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            logged: None,
        }
    }
}

impl Listener for LimitedListener {
    type Io = LimitedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = <TcpListener as Listener>::accept(&mut self.tcp).await;
            if let Ok(permit) = self.permits.clone().try_acquire_owned() {
                return (
                    LimitedStream {
                        stream,
                        _permit: permit,
                    },
                    address,
                );
            }
            // Accept and close excess sockets so the server stays responsive when slots free up.
            drop(stream);
            if self
                .logged
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(60))
            {
                eprintln!("network connection limit reached ({MAX_CONNECTIONS})");
                self.logged = Some(Instant::now());
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

pub(super) struct LimitedStream {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_connections_hold_slots_until_closed() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        let mut listener = LimitedListener::new(tcp);
        let mut held = Vec::new();
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            clients.push(TcpStream::connect(address).await.unwrap());
            held.push(listener.accept().await.0);
        }
        let _excess = TcpStream::connect(address).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
        drop(held.pop());
        let _new = TcpStream::connect(address).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .is_ok()
        );
    }
}
