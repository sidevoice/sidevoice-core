//! Launch configuration from the environment, overridden by command-line flags.

use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

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

impl Config {
    /// Parse `--flag value` pairs over the environment defaults.
    pub fn from_args(args: &[String]) -> io::Result<Self> {
        let mut draft = Draft::from_env();
        for pair in args.chunks(2) {
            let [flag, value] = pair else {
                return Err(invalid_arguments());
            };
            draft.apply(flag, value)?;
        }
        draft.resolve()
    }
}

/// Configuration before validation, with derived paths still unset.
struct Draft {
    data_dir: PathBuf,
    socket: Option<PathBuf>,
    ready_file: Option<PathBuf>,
    host: String,
    port: u16,
    launch_id: String,
    log_file: Option<PathBuf>,
    room_credential: Option<String>,
    idle_exit: f64,
}

impl Draft {
    fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            data_dir: std::env::var_os("SIDEVOICE_CORE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".sidevoice/core")),
            socket: None,
            ready_file: None,
            host: std::env::var("SIDEVOICE_CORE_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
            port: std::env::var("SIDEVOICE_CORE_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8768),
            launch_id: Uuid::new_v4().to_string(),
            log_file: None,
            room_credential: std::env::var("SIDEVOICE_ROOM_CREDENTIAL").ok(),
            idle_exit: std::env::var("SIDEVOICE_CORE_IDLE_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600.0),
        }
    }

    fn apply(&mut self, flag: &str, value: &str) -> io::Result<()> {
        match flag {
            "--data-dir" => self.data_dir = PathBuf::from(value),
            "--socket" => self.socket = Some(PathBuf::from(value)),
            "--ready-file" => self.ready_file = Some(PathBuf::from(value)),
            "--host" => self.host = value.to_owned(),
            "--port" => self.port = value.parse().map_err(|_| invalid_arguments())?,
            "--launch-id" => self.launch_id = value.to_owned(),
            "--log-file" => self.log_file = Some(PathBuf::from(value)),
            "--room-credential" => self.room_credential = Some(value.to_owned()),
            "--idle-exit" => self.idle_exit = value.parse().map_err(|_| invalid_arguments())?,
            _ => return Err(invalid_arguments()),
        }
        Ok(())
    }

    /// Validate, derive the unset paths from the data directory and make every path absolute.
    fn resolve(self) -> io::Result<Config> {
        if !self.idle_exit.is_finite() || self.idle_exit < 0.0 {
            return Err(invalid_arguments());
        }
        let data_dir = absolute(self.data_dir)?;
        let socket = absolute(self.socket.unwrap_or_else(|| data_dir.join("local.sock")))?;
        let ready_file = absolute(
            self.ready_file
                .unwrap_or_else(|| data_dir.join("core.json")),
        )?;
        let log_file = absolute(
            self.log_file
                .unwrap_or_else(|| data_dir.parent().unwrap_or(Path::new(".")).join("core.log")),
        )?;
        Ok(Config {
            data_dir,
            socket,
            ready_file,
            host: self.host,
            port: self.port,
            launch_id: self.launch_id,
            log_file,
            room_credential: self.room_credential,
            idle_exit: self.idle_exit,
        })
    }
}

fn invalid_arguments() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "runtime.arguments")
}

fn absolute(value: PathBuf) -> io::Result<PathBuf> {
    if value.is_absolute() {
        Ok(value)
    } else {
        Ok(std::env::current_dir()?.join(value))
    }
}
