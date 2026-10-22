//! The process lifecycle: claim the directories, load state, serve, report ready, and stop.

use std::io;
use std::sync::Arc;

use tokio::net::TcpListener;

use super::directories::Directories;
use super::event_log::log_event;
use super::failure::StartFailure;
use super::servers::Servers;
use super::{ready, stop, Config};
use crate::control::devices::{DeviceRegistry, NodeIdentity};
use crate::control::room::Room;
use crate::platform::ipc::Channel;
use crate::platform::signal::{Signals, Stop};
use crate::platform::user::{User, UserId};
use crate::server::AppState;
use crate::storage::PrivateDir;

pub(super) async fn serve(config: &Config) -> Result<(), StartFailure> {
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
    let local = config.endpoint.listen().map_err(|error| {
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
    let mut signals = Signals::install();
    let ready = ready::document(config, port, &stored.connector_id, &stored.token);
    let mut servers = Servers::spawn(tcp, local, state.clone());
    let startup = async {
        servers.await_serving(tcp_address, &config.endpoint).await?;
        ready::write(config, dir.path(), &ready)?;
        let _ = log_event(config, "runtime.log_ready", None);
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
    crate::control::telemetry::shutdown().await;
    if crashed {
        Err(StartFailure::crashed())
    } else {
        Ok(())
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
