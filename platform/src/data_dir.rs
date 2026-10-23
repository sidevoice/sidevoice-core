//! `D`, the local data directory of every Sidevoice program on this machine (core, connector, the desktop app's
//! local host): one rule, so nobody recomputes where another one keeps its files.
//!
//! `D` is one directory in the OS's standard place for a program's data (`dirs`):
//!
//! | OS | `D` |
//! |---|---|
//! | macOS | `~/Library/Application Support/Sidevoice` |
//! | Linux | `$XDG_DATA_HOME/sidevoice`, else `~/.local/share/sidevoice` |
//! | Windows | `%LOCALAPPDATA%\Sidevoice` |
//!
//! `$SIDEVOICE_DATA_DIR` (absolute) overrides it: tests, profiles. Layout: `D/core` is the core's directory, `D/install`
//! holds the connector's releases; each program that listens publishes where in a small record under `D`
//! (`core/core.json`, `connector.json`), so none computes a socket path.
//!
//! Nothing differs per OS but the folder's name, so this is a plain module of free functions: no trait, no per-OS files.
//! `use sidevoice_platform::data_dir;` then `data_dir::resolve(|name| std::env::var_os(name))?`.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// The variable that names `D`.
pub const VARIABLE: &str = "SIDEVOICE_DATA_DIR";

/// The name of the program's own directory inside the OS's standard place for a program's data: `Sidevoice`
/// (macOS, Windows), `sidevoice` (Linux, by the XDG convention).
const NAME: &str = if cfg!(target_os = "linux") {
    "sidevoice"
} else {
    "Sidevoice"
};

/// `D` in this OS's standard place for this user, whatever `$SIDEVOICE_DATA_DIR` says; `None` when the user has none.
pub fn standard() -> Option<PathBuf> {
    dirs::data_local_dir()
        .map(|place| place.join(NAME))
        .filter(|path| path.is_absolute())
}

/// `D` as the environment `var` names it: `$SIDEVOICE_DATA_DIR` when set and not empty (it must be absolute), else
/// [`standard`].
pub fn resolve(var: impl Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    match var(VARIABLE).filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{VARIABLE} must be absolute"),
                ))
            }
        }
        None => standard().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "this user has no standard data directory",
            )
        }),
    }
}

/// The core's directory inside `data` (`D/core`).
pub fn core_dir(data: &Path) -> PathBuf {
    data.join("core")
}

/// The connector's releases inside `data` (`D/install`): `current.json` and `releases/<id>`.
pub fn releases_dir(data: &Path) -> PathBuf {
    data.join("install")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn the_default_is_one_directory_in_the_standard_place_and_the_override_wins() {
        let default = resolve(none).unwrap();
        assert_eq!(standard(), Some(default.clone()));
        assert_eq!(default.file_name().unwrap(), NAME);
        assert_eq!(
            default.parent().unwrap(),
            dirs::data_local_dir().unwrap(),
            "the program's own directory, directly in the place for data"
        );
        let elsewhere = std::env::temp_dir().join("elsewhere");
        assert_eq!(
            resolve(|name| (name == VARIABLE).then(|| elsewhere.clone().into_os_string())).unwrap(),
            elsewhere
        );
        assert_eq!(
            resolve(|name| (name == VARIABLE).then(OsString::new)).unwrap(),
            default,
            "an empty override is no override"
        );
        let relative = resolve(|name| (name == VARIABLE).then(|| "relative".into()));
        assert_eq!(relative.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn the_core_and_the_releases_are_inside_the_data_directory() {
        let data = std::env::temp_dir().join("data");
        assert_eq!(core_dir(&data).parent().unwrap(), data);
        assert_eq!(releases_dir(&data).parent().unwrap(), data);
        assert_ne!(core_dir(&data), releases_dir(&data));
    }
}
