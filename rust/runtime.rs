//! Process configuration, launch handshake, listeners and shutdown.

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use axum::Router;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tower::ServiceExt;
use uuid::Uuid;

use crate::control::devices::{DeviceRegistry, NodeIdentity};
use crate::server::{self, AppState};
use crate::storage::PrivateDir;

pub const API: u8 = 1;
pub const CONNECTOR_PROTOCOL: u8 = 2;

pub struct Config {
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    pub ready_file: PathBuf,
    pub host: String,
    pub port: u16,
    pub launch_id: String,
    pub log_file: PathBuf,
    pub room_credential: Option<String>,
    pub idle_exit: f64,
}

fn absolute(value: PathBuf) -> io::Result<PathBuf> {
    if value.is_absolute() {
        Ok(value)
    } else {
        Ok(std::env::current_dir()?.join(value))
    }
}

impl Config {
    pub fn from_args(args: &[String]) -> io::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut data_dir = std::env::var_os("SIDEVOICE_CORE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".sidevoice/core"));
        let mut socket = None;
        let mut ready_file = None;
        let mut host =
            std::env::var("SIDEVOICE_CORE_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
        let mut port = std::env::var("SIDEVOICE_CORE_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8768);
        let mut launch_id = Uuid::new_v4().to_string();
        let mut log_file = None;
        let mut room_credential = std::env::var("SIDEVOICE_ROOM_CREDENTIAL").ok();
        let mut idle_exit: f64 = std::env::var("SIDEVOICE_CORE_IDLE_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600.0);
        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            let value = iter
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "runtime.arguments"))?;
            match flag.as_str() {
                "--data-dir" => data_dir = PathBuf::from(value),
                "--socket" => socket = Some(PathBuf::from(value)),
                "--ready-file" => ready_file = Some(PathBuf::from(value)),
                "--host" => host = value.clone(),
                "--port" => {
                    port = value.parse().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "runtime.arguments")
                    })?
                }
                "--launch-id" => launch_id = value.clone(),
                "--log-file" => log_file = Some(PathBuf::from(value)),
                "--room-credential" => room_credential = Some(value.clone()),
                "--idle-exit" => {
                    idle_exit = value.parse().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "runtime.arguments")
                    })?
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "runtime.arguments",
                    ))
                }
            }
        }
        if !idle_exit.is_finite() || idle_exit < 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime.arguments",
            ));
        }
        let data_dir = absolute(data_dir)?;
        let socket = absolute(socket.unwrap_or_else(|| data_dir.join("local.sock")))?;
        let ready_file = absolute(ready_file.unwrap_or_else(|| data_dir.join("core.json")))?;
        let log_file = absolute(
            log_file
                .unwrap_or_else(|| data_dir.parent().unwrap_or(Path::new(".")).join("core.log")),
        )?;
        Ok(Self {
            data_dir,
            socket,
            ready_file,
            host,
            port,
            launch_id,
            log_file,
            room_credential,
            idle_exit,
        })
    }
}

#[derive(Clone, Debug)]
pub struct StartFailure {
    pub step: &'static str,
    pub key: &'static str,
    pub status: i32,
    pub params: Map<String, Value>,
}

/// A completed startup: both listeners answered health probes and the ready
/// file was written. Connector credentials remain in that private file.
#[derive(Clone, Debug)]
pub struct Ready {
    pub port: u16,
    pub socket: PathBuf,
    pub launch_id: String,
}

impl StartFailure {
    fn new(step: &'static str, key: &'static str) -> Self {
        Self {
            step,
            key,
            status: 0,
            params: Map::new(),
        }
    }
    fn running() -> Self {
        Self {
            step: "bind",
            key: "bind.core-running",
            status: 75,
            params: Map::new(),
        }
    }
}

fn failure(config: &Config, error: &StartFailure) {
    use crate::messages::{render, LocalizedMessage};
    let message = render(
        &LocalizedMessage {
            key: error.key.to_owned(),
            params: error.params.clone(),
        },
        &system_language(),
    );
    let report = json!({"launch_id": config.launch_id, "step": error.step, "key": error.key,
        "message": message, "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)});
    if let Ok(dir) = PrivateDir::open_for_report(&config.data_dir) {
        let _ = dir.write_json("core-failure.json", &report);
    }
    eprintln!("{}", json!({"key": error.key, "message": message}));
    let _ = log_event(config, "runtime.log_failure", Some(error.key));
}

fn log_event(config: &Config, key: &str, cause: Option<&str>) -> io::Result<()> {
    use crate::messages::{render, LocalizedMessage};
    let path = &config.log_file;
    if let Some(parent) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        use std::os::unix::fs::DirBuilderExt;
        builder.recursive(true).mode(0o700).create(parent)?;
    }
    if fs::metadata(path).is_ok_and(|meta| meta.len() >= 5_000_000) {
        let first = path.with_extension("log.1");
        let second = path.with_extension("log.2");
        let _ = fs::remove_file(&second);
        if first.exists() {
            fs::rename(&first, &second)?;
        }
        fs::rename(path, &first)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut message = LocalizedMessage::new(key);
    if let Some(cause) = cause {
        message = message.with_param("key", cause);
    }
    writeln!(
        file,
        "{}",
        json!({"at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "key": key, "message": render(&message, &system_language()),
            "launch_id": config.launch_id})
    )
}

pub fn system_language() -> String {
    std::env::var("LC_ALL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("LANG").ok())
        .unwrap_or_else(|| "en".to_owned())
}

fn remove_own_ready(path: &Path) {
    if fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("pid").and_then(Value::as_u64))
        == Some(std::process::id() as u64)
    {
        let _ = fs::remove_file(path);
    }
}

async fn stop_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM listener");
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("SIGINT listener");
    tokio::select! { _ = term.recv() => (), _ = interrupt.recv() => () }
}

async fn wait_stop(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow() && receiver.changed().await.is_ok() {}
}

// Axum 0.8.9 detaches accepted connection tasks. Keep the same Axum routers
// and Hyper protocol/upgrade driver, but own each connection until it ends.
async fn serve_owned<L: Listener>(
    mut listener: L,
    app: Router,
    stopping: watch::Receiver<bool>,
    force: watch::Receiver<bool>,
) -> io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = wait_stop(stopping.clone()) => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {},
            (io, _) = listener.accept() => {
                let app = app.clone();
                let connection_stop = stopping.clone();
                connections.spawn(async move {
                    let service = hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                        app.clone().oneshot(request.map(axum::body::Body::new))
                    });
                    let mut builder = Builder::new(TokioExecutor::new());
                    builder.http2().enable_connect_protocol();
                    let mut connection = Box::pin(
                        builder.serve_connection_with_upgrades(TokioIo::new(io), service)
                    );
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = wait_stop(connection_stop) => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                    }
                });
            }
        }
    }
    drop(listener);
    while !connections.is_empty() {
        tokio::select! {
            biased;
            _ = wait_stop(force.clone()) => {
                connections.abort_all();
                while connections.join_next().await.is_some() {}
                break;
            }
            _ = connections.join_next() => {},
        }
    }
    Ok(())
}

// The graceful deadline covers both listeners and their accepted connections.
// After it expires, each listener aborts and joins its remaining connections.
async fn retire_listeners(
    stopping: &watch::Sender<bool>,
    force: &watch::Sender<bool>,
    tcp_task: &mut JoinHandle<io::Result<()>>,
    local_task: &mut JoinHandle<io::Result<()>>,
) -> bool {
    let _ = stopping.send(true);
    let wait = async {
        if !tcp_task.is_finished() {
            let _ = (&mut *tcp_task).await;
        }
        if !local_task.is_finished() {
            let _ = (&mut *local_task).await;
        }
    };
    if tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .is_ok()
    {
        return false;
    }
    let _ = force.send(true);
    if !tcp_task.is_finished() {
        let _ = tcp_task.await;
    }
    if !local_task.is_finished() {
        let _ = local_task.await;
    }
    true
}

async fn probe_http<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, path: &str) -> bool {
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    tokio::time::timeout(Duration::from_millis(250), async {
        stream.write_all(request.as_bytes()).await?;
        let mut status = [0u8; 12];
        stream.read_exact(&mut status).await?;
        Ok::<_, io::Error>(&status == b"HTTP/1.1 200")
    })
    .await
    .is_ok_and(|result| result.unwrap_or(false))
}

async fn await_serving_tcp(
    address: std::net::SocketAddr,
    task: &tokio::task::JoinHandle<io::Result<()>>,
) -> Result<(), StartFailure> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let probe_address = if address.ip().is_unspecified() {
        std::net::SocketAddr::new(
            if address.is_ipv4() {
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            } else {
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            },
            address.port(),
        )
    } else {
        address
    };
    loop {
        if task.is_finished() || tokio::time::Instant::now() >= deadline {
            return Err(StartFailure::new("start", "start.failed"));
        }
        if let Ok(Ok(mut stream)) = tokio::time::timeout(
            Duration::from_millis(250),
            TcpStream::connect(probe_address),
        )
        .await
        {
            if probe_http(&mut stream, "/api/rendezvous").await && !task.is_finished() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn await_serving_local(
    path: &Path,
    task: &tokio::task::JoinHandle<io::Result<()>>,
) -> Result<(), StartFailure> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if task.is_finished() || tokio::time::Instant::now() >= deadline {
            return Err(StartFailure::new("start", "start.failed"));
        }
        if let Ok(Ok(mut stream)) =
            tokio::time::timeout(Duration::from_millis(250), UnixStream::connect(path)).await
        {
            if probe_http(&mut stream, "/api/local/health").await && !task.is_finished() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub async fn run(config: Config) -> i32 {
    let (ready, _unused) = oneshot::channel();
    match run_with_shutdown(config, stop_signal(), ready).await {
        Ok(()) => 0,
        Err(error) => error.status,
    }
}

/// Run the same Core service under a host-owned shutdown future. A dropped
/// readiness receiver does not stop Core. The host must signal shutdown and
/// await this future; aborting it skips normal cleanup.
pub async fn run_with_shutdown<S>(
    config: Config,
    shutdown: S,
    ready: oneshot::Sender<Result<Ready, StartFailure>>,
) -> Result<(), StartFailure>
where
    S: Future<Output = ()> + Send,
{
    let mut ready = Some(ready);
    if log_event(&config, "runtime.log_start", None).is_err() {
        let error = StartFailure::new("start", "start.failed");
        failure(&config, &error);
        if let Some(ready) = ready.take() {
            let _ = ready.send(Err(error.clone()));
        }
        return Err(error);
    }
    match serve(&config, shutdown, &mut ready).await {
        Ok(()) => {
            let _ = log_event(&config, "runtime.log_stop", None);
            Ok(())
        }
        Err(error) => {
            if error.step != "run" {
                failure(&config, &error);
            }
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(error.clone()));
            }
            Err(error)
        }
    }
}

async fn serve<S>(
    config: &Config,
    shutdown: S,
    ready_signal: &mut Option<oneshot::Sender<Result<Ready, StartFailure>>>,
) -> Result<(), StartFailure>
where
    S: Future<Output = ()> + Send,
{
    let dir = PrivateDir::open(&config.data_dir)
        .map_err(|_| StartFailure::new("directory", "identity.unsafe-directory"))?;
    let socket_parent = config
        .socket
        .parent()
        .ok_or_else(|| StartFailure::new("directory", "identity.unsafe-directory"))?;
    let socket_dir = PrivateDir::open(socket_parent)
        .map_err(|_| StartFailure::new("directory", "identity.unsafe-directory"))?;
    let first_lock = dir
        .lock()
        .map_err(|_| StartFailure::new("bind", "start.failed"))?
        .ok_or_else(StartFailure::running)?;
    let second_lock =
        if fs::canonicalize(dir.path()).ok() == fs::canonicalize(socket_dir.path()).ok() {
            None
        } else {
            Some(
                socket_dir
                    .lock()
                    .map_err(|_| StartFailure::new("bind", "start.failed"))?
                    .ok_or_else(StartFailure::running)?,
            )
        };
    let _held = (first_lock, second_lock);
    let _ = dir.remove("core-failure.json");
    let identity = NodeIdentity::load_or_create(&dir)
        .map_err(|_| StartFailure::new("identity", "identity.unreadable"))?;
    let registry = DeviceRegistry::load(dir.clone())
        .map_err(|_| StartFailure::new("start", "start.failed"))?;
    let room = std::sync::Arc::new(
        crate::control::room::Room::load(dir.clone())
            .map_err(|_| StartFailure::new("start", "start.failed"))?,
    );
    let (connector_id, token) = room
        .local_credential()
        .map_err(|_| StartFailure::new("start", "start.failed"))?;
    let tcp = TcpListener::bind((config.host.as_str(), config.port))
        .await
        .map_err(|_| StartFailure::new("bind", "bind.port-in-use"))?;
    let port = tcp
        .local_addr()
        .map_err(|_| StartFailure::new("bind", "start.failed"))?
        .port();
    if let Ok(found) = fs::symlink_metadata(&config.socket) {
        if !found.file_type().is_socket() {
            return Err(StartFailure::new("bind", "bind.port-in-use"));
        }
        match UnixStream::connect(&config.socket).await {
            Ok(_) => return Err(StartFailure::new("bind", "bind.port-in-use")),
            Err(error)
                if error.kind() == io::ErrorKind::ConnectionRefused
                    || error.kind() == io::ErrorKind::NotFound =>
            {
                fs::remove_file(&config.socket)
                    .map_err(|_| StartFailure::new("bind", "start.failed"))?;
            }
            Err(_) => return Err(StartFailure::new("bind", "bind.port-in-use")),
        }
    }
    let local = UnixListener::bind(&config.socket)
        .map_err(|_| StartFailure::new("bind", "bind.port-in-use"))?;
    let cleanup = SocketCleanup::new(config.socket.clone());
    fs::set_permissions(&config.socket, fs::Permissions::from_mode(0o600))
        .map_err(|_| StartFailure::new("bind", "start.failed"))?;
    let machine_host = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let rendezvous = server::rendezvous::Rendezvous::new(
        config.room_credential.as_ref().map(PathBuf::from),
        url::Url::parse(&format!("http://127.0.0.1:{port}/"))
            .map_err(|_| StartFailure::new("start", "start.failed"))?,
        machine_host.clone(),
        room.clone(),
    );
    let state = Arc::new(AppState::new(
        dir.clone(),
        identity,
        registry,
        config.launch_id.clone(),
        machine_host,
        port,
        room.clone(),
        rendezvous.clone(),
    ));
    let ready = json!({"pid": std::process::id(), "port": port, "url": format!("http://127.0.0.1:{port}"),
        "socket": config.socket, "launch_id": config.launch_id, "version": env!("CARGO_PKG_VERSION"),
        "api": API, "protocol": CONNECTOR_PROTOCOL, "connector_protocols": [CONNECTOR_PROTOCOL, 3],
        "connector_id": connector_id, "token": token});
    let tcp_address = tcp
        .local_addr()
        .map_err(|_| StartFailure::new("start", "start.failed"))?;
    let mut delivery_task = tokio::spawn(room.pump());
    let (stopping, receiver) = watch::channel(false);
    let (force, force_receiver) = watch::channel(false);
    let tcp_app = server::router(state.clone(), false);
    let local_app = server::router(state.clone(), true);
    let tcp_receiver = receiver.clone();
    let tcp_task = tokio::spawn(serve_owned(
        tcp,
        tcp_app,
        tcp_receiver,
        force_receiver.clone(),
    ));
    let local_task = tokio::spawn(serve_owned(local, local_app, receiver, force_receiver));
    let mut tcp_task = tcp_task;
    let mut local_task = local_task;
    let startup = async {
        await_serving_tcp(tcp_address, &tcp_task).await?;
        await_serving_local(&config.socket, &local_task).await?;
        if tcp_task.is_finished() || local_task.is_finished() {
            return Err(StartFailure::new("start", "start.failed"));
        }
        let ready_dir = PrivateDir::open(config.ready_file.parent().unwrap_or(dir.path()))
            .map_err(|_| StartFailure::new("start", "start.failed"))?;
        ready_dir
            .write_json(
                config
                    .ready_file
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("core.json"),
                &ready,
            )
            .map_err(|_| StartFailure::new("start", "start.failed"))?;
        let _ = log_event(config, "runtime.log_ready", None);
        if let Some(ready) = ready_signal.take() {
            let _ = ready.send(Ok(Ready {
                port,
                socket: config.socket.clone(),
                launch_id: config.launch_id.clone(),
            }));
        }
        Ok::<_, StartFailure>(())
    }
    .await;
    if let Err(error) = startup {
        retire_listeners(&stopping, &force, &mut tcp_task, &mut local_task).await;
        delivery_task.abort();
        let _ = delivery_task.await;
        return Err(error);
    }
    let mut rendezvous_task = tokio::spawn(rendezvous.clone().run());
    let crashed = tokio::select! {
        _ = shutdown => false,
        _ = &mut tcp_task => true,
        _ = &mut local_task => true,
        _ = watch_idle(state, config.idle_exit), if config.idle_exit > 0.0 => false,
    };
    let _ = stopping.send(true);
    rendezvous.stop();
    if tokio::time::timeout(Duration::from_secs(10), &mut rendezvous_task)
        .await
        .is_err()
    {
        rendezvous_task.abort();
        let _ = rendezvous_task.await;
    }
    let forced = retire_listeners(&stopping, &force, &mut tcp_task, &mut local_task).await;
    remove_own_ready(&config.ready_file);
    drop(cleanup);
    delivery_task.abort();
    let _ = delivery_task.await;
    if crashed || forced {
        Err(StartFailure {
            step: "run",
            key: "start.failed",
            status: 1,
            params: Map::new(),
        })
    } else {
        Ok(())
    }
}

async fn watch_idle(state: Arc<AppState>, seconds: f64) {
    let quiet = Duration::from_secs_f64(seconds.max(0.01));
    let mut since = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs_f64(seconds.clamp(0.01, 5.0))).await;
        if state.open_calls() > 0 {
            since = tokio::time::Instant::now();
        } else if since.elapsed() >= quiet {
            return;
        }
    }
}

struct SocketCleanup {
    path: PathBuf,
    inode: Option<u64>,
}

impl SocketCleanup {
    fn new(path: PathBuf) -> Self {
        use std::os::unix::fs::MetadataExt;
        let inode = fs::metadata(&path).ok().map(|meta| meta.ino());
        Self { path, inode }
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if self.inode.is_some()
            && fs::metadata(&self.path).ok().map(|meta| meta.ino()) == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
