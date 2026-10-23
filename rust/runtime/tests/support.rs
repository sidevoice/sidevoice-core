use std::ffi::OsString;
use std::path::PathBuf;

use crate::platform::ipc::{Address, Endpoint};
use crate::runtime::Config;

pub(super) fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

/// An absolute path on any OS, where nothing is ever created: the configuration only derives paths.
pub(super) fn root() -> PathBuf {
    std::env::temp_dir().join("sidevoice-config-test")
}

/// `root()/<relative>` as the text of a flag.
pub(super) fn flag(relative: &str) -> String {
    root().join(relative).to_str().unwrap().to_owned()
}

/// The local channel the core derives for the data directory `root()/core`.
pub(super) fn core_endpoint() -> String {
    Endpoint::new(&root().join("core"), "local").to_string()
}

/// A configuration that sets every flag, so no environment default leaks in.
pub(super) fn full_config() -> Config {
    Config::from_args(args(&[
        "--data-dir",
        &flag("core"),
        "--ready-file",
        &flag("run/ready.json"),
        "--host",
        "0.0.0.0",
        "--port",
        "9000",
        "--launch-id",
        "launch",
        "--log-file",
        &flag("log/core.log"),
        "--idle-exit",
        "0",
    ]))
    .expect("valid flags")
}
