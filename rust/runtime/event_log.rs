//! The private, size-rotated process log of lifecycle events.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde_json::json;

use super::{system_language, Config};
use crate::messages::{render, LocalizedMessage};

const ROTATE_AT_BYTES: u64 = 5_000_000;

/// Append one localized event line; `cause` becomes the message's `key` parameter.
pub(super) fn log_event(config: &Config, key: &str, cause: Option<&str>) -> io::Result<()> {
    let path = &config.log_file;
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    rotate_if_full(path)?;
    let mut file = open_private_append(path)?;
    let mut message = LocalizedMessage::new(key);
    if let Some(cause) = cause {
        message = message.with_param("key", cause);
    }
    writeln!(
        file,
        "{}",
        json!({"at": timestamp(), "key": key, "message": render(&message, &system_language()),
            "launch_id": config.launch_id})
    )
}

/// The current UTC time as RFC 3339 with milliseconds.
pub(super) fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Keep two older generations, `<log>.1` and `<log>.2`, once the log reaches its size limit.
pub(super) fn rotate_if_full(path: &Path) -> io::Result<()> {
    if !fs::metadata(path).is_ok_and(|meta| meta.len() >= ROTATE_AT_BYTES) {
        return Ok(());
    }
    let first = path.with_extension("log.1");
    let second = path.with_extension("log.2");
    let _ = fs::remove_file(&second);
    if first.exists() {
        fs::rename(&first, &second)?;
    }
    fs::rename(path, &first)
}

fn open_private_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
