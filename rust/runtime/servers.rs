//! The TCP and local HTTP servers: spawn, readiness probes, and graceful stop.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::failure::StartFailure;
use crate::server::{self, AppState};

const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const PROBE_INTERVAL: Duration = Duration::from_millis(10);
const START_DEADLINE: Duration = Duration::from_secs(5);
const STOP_GRACE: Duration = Duration::from_secs(10);

type ServerTask = JoinHandle<io::Result<()>>;

pub(super) struct Servers {
    stopping: watch::Sender<bool>,
    tcp: ServerTask,
    local: ServerTask,
}

impl Servers {
    pub(super) fn spawn(tcp: TcpListener, local: UnixListener, state: Arc<AppState>) -> Self {
        let (stopping, receiver) = watch::channel(false);
        let tcp_app = server::router(state.clone(), false);
        let local_app = server::router(state, true);
        let tcp_receiver = receiver.clone();
        let tcp = tokio::spawn(async move {
            axum::serve(tcp, tcp_app)
                .with_graceful_shutdown(wait_stop(tcp_receiver))
                .await
        });
        let local = tokio::spawn(async move {
            axum::serve(local, local_app)
                .with_graceful_shutdown(wait_stop(receiver))
                .await
        });
        Self {
            stopping,
            tcp,
            local,
        }
    }

    /// Wait until both servers answer their health route, or fail if either stops first.
    pub(super) async fn await_serving(
        &self,
        tcp_address: SocketAddr,
        socket: &Path,
    ) -> Result<(), StartFailure> {
        let probe_address = loopback_for(tcp_address);
        await_serving(&self.tcp, || async move {
            match tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(probe_address)).await {
                Ok(Ok(stream)) => probe_http(stream, "/api/rendezvous").await,
                _ => false,
            }
        })
        .await?;
        await_serving(&self.local, || async move {
            match tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(socket)).await {
                Ok(Ok(stream)) => probe_http(stream, "/api/local/health").await,
                _ => false,
            }
        })
        .await?;
        if self.tcp.is_finished() || self.local.is_finished() {
            return Err(StartFailure::new("start", "start.failed"));
        }
        Ok(())
    }

    /// Resolve when either server task ends.
    pub(super) async fn exited(&mut self) {
        tokio::select! { _ = &mut self.tcp => (), _ = &mut self.local => () }
    }

    /// Ask both servers to shut down gracefully.
    pub(super) fn stop(&self) {
        let _ = self.stopping.send(true);
    }

    /// Wait a bounded time for both servers to finish.
    pub(super) async fn join(self) {
        let Self { tcp, local, .. } = self;
        let _ = tokio::time::timeout(STOP_GRACE, async {
            if !tcp.is_finished() {
                let _ = tcp.await;
            }
            if !local.is_finished() {
                let _ = local.await;
            }
        })
        .await;
    }
}

async fn wait_stop(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow() && receiver.changed().await.is_ok() {}
}

/// Retry `probe` until it reports a served request, while the task runs and the deadline holds.
async fn await_serving<P, F>(task: &ServerTask, probe: P) -> Result<(), StartFailure>
where
    P: Fn() -> F,
    F: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + START_DEADLINE;
    loop {
        if task.is_finished() || tokio::time::Instant::now() >= deadline {
            return Err(StartFailure::new("start", "start.failed"));
        }
        if probe().await && !task.is_finished() {
            return Ok(());
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// An unspecified bind address is probed on the loopback of the same family.
pub(super) fn loopback_for(address: SocketAddr) -> SocketAddr {
    if !address.ip().is_unspecified() {
        return address;
    }
    let ip = if address.is_ipv4() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    };
    SocketAddr::new(ip, address.port())
}

async fn probe_http<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, path: &str) -> bool {
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    tokio::time::timeout(PROBE_TIMEOUT, async {
        stream.write_all(request.as_bytes()).await?;
        let mut status = [0u8; 12];
        stream.read_exact(&mut status).await?;
        Ok::<_, io::Error>(&status == b"HTTP/1.1 200")
    })
    .await
    .is_ok_and(|result| result.unwrap_or(false))
}
