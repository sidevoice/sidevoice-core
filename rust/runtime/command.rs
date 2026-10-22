//! What the process was asked to do, from its command line and environment.
//!
//! Like the connector's, the command line is English only and is not translated: `--help`, usage and
//! refusals are clap's. A flag that is not given falls back to its environment variable, then to its default.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

/// Sidevoice core: this node's conversations.
#[derive(Debug, Parser)]
#[command(name = "sidevoice-core", disable_version_flag = true)]
pub struct Cli {
    /// Address to listen on [default: 127.0.0.1]
    #[arg(long, env = "SIDEVOICE_CORE_HOST", value_name = "HOST")]
    pub host: Option<String>,
    /// Port to listen on; 0 chooses a free port [default: 10829]
    #[arg(long, env = "SIDEVOICE_CORE_PORT", value_name = "PORT")]
    pub port: Option<u16>,
    /// Private core data directory [default: ~/.sidevoice/core]
    #[arg(long, env = "SIDEVOICE_CORE_DATA_DIR", value_name = "PATH")]
    pub data_dir: Option<PathBuf>,
    /// Where to listen for this machine's own processes, as the ready file's `endpoint` records it [default:
    /// derived from the data directory]
    #[arg(long, value_name = "ENDPOINT")]
    #[cfg_attr(unix, arg(conflicts_with = "socket"))]
    pub endpoint: Option<String>,
    /// Deprecated, Unix only: the local socket path; use `--endpoint`
    #[cfg(unix)]
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Readiness file path [default: core.json in the data directory]
    #[arg(long, value_name = "PATH")]
    pub ready_file: Option<PathBuf>,
    /// Identifier for this launch [default: a new one]
    #[arg(long, value_name = "ID")]
    pub launch_id: Option<String>,
    /// Log path [default: core.log beside the data directory]
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,
    /// Exit after this many idle seconds; 0 disables [default: 600]
    #[arg(
        long,
        env = "SIDEVOICE_CORE_IDLE_SECONDS",
        value_name = "SECONDS",
        value_parser = idle_seconds,
        allow_hyphen_values = true
    )]
    pub idle_exit: Option<f64>,
}

fn idle_seconds(value: &str) -> Result<f64, String> {
    match value.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() && seconds >= 0.0 => Ok(seconds),
        _ => Err("expected a finite number of seconds, 0 or more".to_owned()),
    }
}

impl Cli {
    /// Parse the arguments after the program name.
    pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Self, clap::Error> {
        Self::try_parse_from(std::iter::once(OsString::from("sidevoice-core")).chain(args))
    }
}
