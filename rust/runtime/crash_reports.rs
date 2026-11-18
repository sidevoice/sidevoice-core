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
