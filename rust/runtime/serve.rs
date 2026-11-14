//! The process lifecycle: claim the directories, load state, serve, report ready, and stop.

use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::TcpListener;
use tracing::info;

use super::directories::Directories;
use super::failure::StartFailure;
use super::servers::Servers;
use super::{ready, stop, Config};
use crate::control::devices::{DeviceRegistry, NodeIdentity};
use crate::control::room::Room;
use crate::log_safe::ClientId;
use crate::platform::ipc::{Channel, Channels};
use crate::platform::signal::{Signals, Stop};
use crate::platform::user::{User, UserId};
use crate::server::AppState;
use crate::storage::PrivateDir;

pub(super) async fn serve(config: &Config) -> Result<(), StartFailure> {
    let started = Instant::now();
    let directories = Directories::lock(config)?;
    let dir = &directories.data;
    let _ = dir.remove("core-failure.json");
    let stored = Stored::load(dir)?;
    let tcp = TcpListener::bind((config.host.as_str(), config.port))
        .await
        .map_err(|_| StartFailure::new("bind", "bind.port-in-use"))?;
    let tcp_address = tcp
        .local_addr()
        .map_err(|_| StartFailure::new("bind", "start.failed"))?;
    let port = tcp_address.port();
    let local = Channels::listen(&config.endpoint).map_err(|error| {
        let taken = matches!(
            error.kind(),
            io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
        );
        StartFailure::new(
            "bind",
            if taken {
                "bind.port-in-use"
            } else {
                "start.failed"
            },
        )
    })?;
    let machine_host = UserId::host_name();
    let state = Arc::new(AppState::new(
        dir.clone(),
        stored.identity,
        stored.registry,
        config.launch_id.clone(),
        machine_host,
        config.host.clone(),
        port,
        stored.room.clone(),
    ));
    let delivery_task = tokio::spawn(stored.room.pump());
    // Conversations whose last viewer aged out or was unpaired close here, at startup and every quarter hour.
    let departed_task = tokio::spawn({
        let state = state.clone();
        async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
            loop {
                tick.tick().await;
                state.close_departed_conversations().await;
            }
        }
    });
    let mut signals = Signals::install();
    let ready = ready::document(config, port, &stored.connector_id, &stored.token);
    let mut servers = Servers::spawn(tcp, local, state.clone());
    let startup = async {
        servers.await_serving(tcp_address, &config.endpoint).await?;
        ready::write(config, dir.path(), &ready)?;
        info!(
            key = "runtime.log_ready",
            launch_id = %ClientId(&config.launch_id),
            port,
            bind = bind_class(&config.host),
            duration_ms = started.elapsed().as_millis() as u64,
            "core ready"
        );
        Ok::<_, StartFailure>(())
    }
    .await;
    if let Err(error) = startup {
        servers.stop();
        servers.join().await;
        return Err(error);
    }
    let crashed = tokio::select! {
        _ = signals.received() => false,
        _ = state.shutdown_requested() => false,
        _ = servers.exited() => true,
        _ = stop::idle_for(state.clone(), config.idle_exit), if config.idle_exit > 0.0 => false,
    };
    servers.stop();
    servers.join().await;
    ready::remove_own(&config.ready_file);
    delivery_task.abort();
    departed_task.abort();
    crate::control::telemetry::shutdown().await;
    if crashed {
        Err(StartFailure::crashed())
    } else {
        Ok(())
    }
}

/// What the listening address is, never the address: a LAN address can name a person's network.
fn bind_class(host: &str) -> &'static str {
    match host.parse::<IpAddr>() {
        Ok(ip) if ip.is_loopback() => "loopback",
        Ok(_) => "network",
        Err(_) => "name",
    }
}

/// The node's persisted state, loaded before anything listens.
struct Stored {
    identity: NodeIdentity,
    registry: DeviceRegistry,
    room: Arc<Room>,
    connector_id: String,
    token: String,
}

impl Stored {
    fn load(dir: &PrivateDir) -> Result<Self, StartFailure> {
        let start_failed = |_| StartFailure::new("start", "start.failed");
        let identity = NodeIdentity::load_or_create(dir)
            .map_err(|_| StartFailure::new("identity", "identity.unreadable"))?;
        let registry = DeviceRegistry::load(dir.clone()).map_err(start_failed)?;
        let room = Arc::new(Room::load(dir.clone()).map_err(start_failed)?);
        let (connector_id, token) = room.local_credential().map_err(start_failed)?;
        Ok(Self {
            identity,
            registry,
            room,
            connector_id,
            token,
        })
    }
}
