//! Launch configuration: command-line flags over the environment over the defaults.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::platform::data_dir;
use crate::platform::ipc::{Address, Endpoint};

use super::Cli;

pub struct Config {
    pub data_dir: PathBuf,
    pub endpoint: Endpoint,
    pub ready_file: PathBuf,
    pub host: String,
    pub port: u16,
    pub launch_id: String,
    pub log_file: PathBuf,
    pub idle_exit: f64,
}

impl Config {
    /// Parse the arguments after the program name over the environment defaults.
    pub fn from_args(args: impl IntoIterator<Item = OsString>) -> Result<Self, clap::Error> {
        let cli = Cli::parse_args(args)?;
        Self::from_cli(cli).map_err(|error| {
            use clap::CommandFactory;
            Cli::command().error(clap::error::ErrorKind::Io, error)
        })
    }

    /// Derive the unset paths from the data directory and make every path absolute.
    pub fn from_cli(cli: Cli) -> io::Result<Self> {
        // Unnamed, the core lives in `D/core`, `D` being the one data directory every Sidevoice program shares.
        let data_dir = absolute(match cli.data_dir {
            Some(dir) => dir,
            None => data_dir::core_dir(&data_dir::resolve(|name| std::env::var_os(name))?),
        })?;
        // The local channel is derived from the data directory alone, so the data directory's lock holds it too.
        let endpoint = Endpoint::new(&data_dir, "local");
        let ready_file = absolute(cli.ready_file.unwrap_or_else(|| data_dir.join("core.json")))?;
        let log_file = absolute(
            cli.log_file
                .unwrap_or_else(|| data_dir.parent().unwrap_or(Path::new(".")).join("core.log")),
        )?;
        Ok(Self {
            data_dir,
            endpoint,
            ready_file,
            host: cli.host.unwrap_or_else(|| "127.0.0.1".to_owned()),
            port: cli.port.unwrap_or(10829),
            launch_id: cli.launch_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            log_file,
            idle_exit: cli.idle_exit.unwrap_or(600.0),
        })
    }
}

fn absolute(value: PathBuf) -> io::Result<PathBuf> {
    if value.is_absolute() {
        Ok(value)
    } else {
        Ok(std::env::current_dir()?.join(value))
    }
}
