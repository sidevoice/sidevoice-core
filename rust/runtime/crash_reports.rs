//! `sidevoice-core crash-reports on|off|status`: the machine's "send crash reports" setting from the command line.
//!
//! It writes the file the running core reads again whenever it matters (`rust/crash/settings.rs`), so it needs no running
//! core and tells none: a change here never reaches the desktop app, and the app's change through the local endpoint is the
//! same file. Off erases the machine id and every pending record, as the endpoint does.

use std::io;

use super::command::Switch;
use super::Config;
use crate::storage::PrivateDir;

/// Applies `action` to this machine's setting and says what it is now: `on` or `off`.
pub fn crash_reports(config: &Config, action: Switch) -> io::Result<&'static str> {
    let dir = PrivateDir::open(&config.data_dir)?;
    match action {
        Switch::On => crate::crash::set_enabled(dir.path(), true)?,
        Switch::Off => crate::crash::set_enabled(dir.path(), false)?,
        Switch::Status => {}
    }
    Ok(if crate::crash::setting(dir.path())? {
        "on"
    } else {
        "off"
    })
}

/// `sidevoice-core record-exit`: writes the exit file when the service manager says the core ended in a crash status, for the next
/// start to read (`crash::exit`). `file` is where; by default the core's own under the data directory. Nothing else is read or kept.
pub fn record_exit(file: Option<std::path::PathBuf>) -> io::Result<bool> {
    use sidevoice_platform::exit;
    let path = match file {
        Some(path) => path,
        None => {
            let data = crate::platform::data_dir::resolve(|name| std::env::var_os(name))?;
            exit::exit_file(&data, &exit::CORE)
        }
    };
    exit::record_systemd(&path, |name| std::env::var(name).ok())
}
