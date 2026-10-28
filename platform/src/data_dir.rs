//! `D`, the local data directory of every Sidevoice program on this machine (core, connector, the desktop app's
//! local host): one rule, so nobody recomputes where another one keeps its files.
//!
//! `D` is one directory in the OS's standard place for a program's data (`dirs`):
//!
//! | OS | `D` |
//! |---|---|
//! | macOS | `~/Library/Application Support/Sidevoice` |
//! | Linux | `$XDG_DATA_HOME/sidevoice`, else `~/.local/share/sidevoice` |
//! | Windows | `%LOCALAPPDATA%\Sidevoice\Data` |
//!
//! `$SIDEVOICE_DATA_DIR` (absolute) overrides it: tests, profiles. Layout: `D/core` is the core's directory, `D/install`
//! holds the connector's releases; each program that listens publishes where in a small record under `D`
//! (`core/core.json`, `connector.json`), so none computes a socket path.
//!
//! On Windows `%LOCALAPPDATA%\Sidevoice` is the desktop app's own install folder, so `D` is the `Data` subfolder of it:
//! removing the data never touches the app.
//!
//! The models the engine downloads and the desktop app shares are NOT in `D`: `Models` is always in the app's folder
//! beside `D` ([`models_dir`]; on Windows `%LOCALAPPDATA%\Sidevoice\Models`), whatever `$SIDEVOICE_DATA_DIR` says, one content-addressed cache for every Sidevoice instance
//! of this user. The connector never removes it.
//!
//! Nothing differs per OS but the folder's name and, on Windows, the `Data` level, so this is a plain module of free functions: no trait, no per-OS files.
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

/// The program's own folder in the OS's standard place for a program's data (on Windows also where the desktop app
/// installs); `None` when the user has none.
fn app_dir() -> Option<PathBuf> {
    dirs::data_local_dir()
        .map(|place| place.join(NAME))
        .filter(|path| path.is_absolute())
}

/// `D` in this OS's standard place for this user, whatever `$SIDEVOICE_DATA_DIR` says; `None` when the user has none.
/// On Windows it is the `Data` subfolder of the app's folder.
pub fn standard() -> Option<PathBuf> {
    app_dir().map(|dir| if cfg!(windows) { dir.join("Data") } else { dir })
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

/// The name of the shared models folder. It is the engine's and the desktop app's, never the connector's:
/// uninstalling the connector leaves it.
pub const MODELS: &str = "Models";

/// The shared models folder: `Models` in the program's folder of the OS's standard place, beside `D` (on Windows
/// `Sidevoice\Models`, not `Sidevoice\Data\Models`), the same rule on every OS
/// (Linux too) and independent of `$SIDEVOICE_DATA_DIR`, so every Sidevoice instance of this user shares one cache of
/// downloaded models. `None` when the user has no standard data directory.
pub fn models_dir() -> Option<PathBuf> {
    app_dir().map(|dir| dir.join(MODELS))
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
        let app = dirs::data_local_dir().unwrap().join(NAME);
        if cfg!(windows) {
            assert_eq!(default, app.join("Data"), "a subfolder of the app's folder");
        } else {
            assert_eq!(
                default, app,
                "the program's own directory in the place for data"
            );
        }
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
    fn the_models_are_in_the_standard_place_whatever_the_override_says() {
        let models = models_dir().unwrap();
        assert_eq!(models, app_dir().unwrap().join("Models"));
        assert_eq!(models.file_name().unwrap(), MODELS);
        assert!(models.is_absolute());
        let elsewhere = std::env::temp_dir().join("elsewhere");
        let data =
            resolve(|name| (name == VARIABLE).then(|| elsewhere.clone().into_os_string())).unwrap();
        assert!(
            !models.starts_with(&data),
            "an override of D does not move the models"
        );
    }

    #[test]
    fn the_core_and_the_releases_are_inside_the_data_directory() {
        let data = std::env::temp_dir().join("data");
        assert_eq!(core_dir(&data).parent().unwrap(), data);
        assert_eq!(releases_dir(&data).parent().unwrap(), data);
        assert_ne!(core_dir(&data), releases_dir(&data));
    }
}
