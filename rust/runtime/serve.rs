//! The process lifecycle: claim the directories, load state, serve, report ready, and stop.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::directories::Directories;
use super::event_log::log_event;
use super::failure::StartFailure;
use super::servers::Servers;
use super::{local_socket, ready, stop, Config};
use crate::control::devices::{DeviceRegistry, NodeIdentity};
use crate::control::room::Room;
use crate::server::rendezvous::Rendezvous;
use crate::server::AppState;
use crate::storage::PrivateDir;

const RENDEZVOUS_GRACE: Duration = Duration::from_secs(10);

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
    let (local, cleanup) = local_socket::bind(&config.socket).await?;
    let machine_host = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let rendezvous = Rendezvous::new(
        config.room_credential.as_ref().map(PathBuf::from),
        url::Url::parse(&format!("http://127.0.0.1:{port}/"))
            .map_err(|_| StartFailure::new("start", "start.failed"))?,
        machine_host.clone(),
        stored.room.clone(),
    );
    let state = Arc::new(AppState::new(
        dir.clone(),
        stored.identity,
        stored.registry,
        config.launch_id.clone(),
        machine_host,
        port,
        stored.room.clone(),
        rendezvous.clone(),
    ));
    let delivery_task = tokio::spawn(stored.room.pump());
    let ready = ready::document(config, port, &stored.connector_id, &stored.token);
    let mut servers = Servers::spawn(tcp, local, state.clone());
    let startup = async {
        servers.await_serving(tcp_address, &config.socket).await?;
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
    let rendezvous_task = tokio::spawn(rendezvous.clone().run());
    let crashed = tokio::select! {
        _ = stop::signal_received() => false,
        _ = servers.exited() => true,
        _ = stop::idle_for(state, config.idle_exit), if config.idle_exit > 0.0 => false,
    };
    servers.stop();
    rendezvous.stop();
    join_or_abort(rendezvous_task).await;
    servers.join().await;
    ready::remove_own(&config.ready_file);
    drop(cleanup);
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

async fn join_or_abort(mut task: JoinHandle<()>) {
    if tokio::time::timeout(RENDEZVOUS_GRACE, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}
