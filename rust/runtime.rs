//! Process configuration, launch handshake, listeners and shutdown.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::watch;
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
        Ok(Self {
            data_dir,
            socket,
            ready_file,
            host,
            port,
            launch_id,
            idle_exit,
        })
    }
}

pub struct StartFailure {
    pub step: &'static str,
    pub key: &'static str,
    pub status: i32,
    pub params: Map<String, Value>,
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

pub async fn run(config: Config) -> i32 {
    match serve(&config).await {
        Ok(()) => 0,
        Err(error) => {
            if error.step != "run" {
                failure(&config, &error);
            }
            error.status
        }
    }
}

async fn serve(config: &Config) -> Result<(), StartFailure> {
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
    let (connector_id, token) = dir
        .connector_credential()
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
    let state = Arc::new(AppState::new(
        dir.clone(),
        identity,
        registry,
        config.launch_id.clone(),
        machine_host,
        port,
    ));
    let ready = json!({"pid": std::process::id(), "port": port, "url": format!("http://127.0.0.1:{port}"),
        "socket": config.socket, "launch_id": config.launch_id, "version": env!("CARGO_PKG_VERSION"),
        "api": API, "protocol": CONNECTOR_PROTOCOL, "connector_protocols": [CONNECTOR_PROTOCOL, 3],
        "connector_id": connector_id, "token": token});
    let ready_dir = PrivateDir::open(config.ready_file.parent().unwrap_or(dir.path()))
        .map_err(|_| StartFailure::new("start", "start.failed"))?;
    if ready_dir
        .write_json(
            config
                .ready_file
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("core.json"),
            &ready,
        )
        .is_err()
    {
        return Err(StartFailure::new("start", "start.failed"));
    }
    let (stopping, receiver) = watch::channel(false);
    let tcp_app = server::router(state.clone(), false);
    let local_app = server::router(state.clone(), true);
    let tcp_receiver = receiver.clone();
    let tcp_task = tokio::spawn(async move {
        axum::serve(tcp, tcp_app)
            .with_graceful_shutdown(wait_stop(tcp_receiver))
            .await
    });
    let local_task = tokio::spawn(async move {
        axum::serve(local, local_app)
            .with_graceful_shutdown(wait_stop(receiver))
            .await
    });
    let mut tcp_task = tcp_task;
    let mut local_task = local_task;
    let crashed = tokio::select! {
        _ = stop_signal() => false,
        _ = &mut tcp_task => true,
        _ = &mut local_task => true,
        _ = watch_idle(state, config.idle_exit), if config.idle_exit > 0.0 => false,
    };
    let _ = stopping.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        if !tcp_task.is_finished() {
            let _ = tcp_task.await;
        }
        if !local_task.is_finished() {
            let _ = local_task.await;
        }
    })
    .await;
    remove_own_ready(&config.ready_file);
    drop(cleanup);
    if crashed {
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
