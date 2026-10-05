use std::io;
use std::path::PathBuf;

use super::support::{args, full_config};
use crate::runtime::Config;

#[test]
fn every_flag_overrides_its_setting() {
    let config = full_config();
    assert_eq!(config.data_dir, PathBuf::from("/srv/sidevoice/core"));
    assert_eq!(config.socket, PathBuf::from("/run/sidevoice/local.sock"));
    assert_eq!(
        config.ready_file,
        PathBuf::from("/run/sidevoice/ready.json")
    );
    assert_eq!(config.host, "0.0.0.0");
    assert_eq!(config.port, 9000);
    assert_eq!(config.launch_id, "launch");
    assert_eq!(
        config.log_file,
        PathBuf::from("/var/log/sidevoice/core.log")
    );
    assert_eq!(
        config.room_credential.as_deref(),
        Some("/srv/sidevoice/room.json")
    );
    assert_eq!(config.idle_exit, 0.0);
}

#[test]
fn unset_paths_derive_from_the_data_directory() {
    let config = Config::from_args(&args(&["--data-dir", "/srv/sidevoice/core"])).unwrap();
    assert_eq!(
        config.socket,
        PathBuf::from("/srv/sidevoice/core/local.sock")
    );
    assert_eq!(
        config.ready_file,
        PathBuf::from("/srv/sidevoice/core/core.json")
    );
    assert_eq!(config.log_file, PathBuf::from("/srv/sidevoice/core.log"));
}

#[test]
fn relative_paths_are_made_absolute() {
    let config = Config::from_args(&args(&["--data-dir", "relative/core"])).unwrap();
    assert_eq!(
        config.data_dir,
        std::env::current_dir().unwrap().join("relative/core")
    );
}

#[test]
fn invalid_arguments_are_refused_with_the_runtime_key() {
    for invalid in [
        &["--port"][..],
        &["--unknown", "value"],
        &["--port", "not-a-port"],
        &["--idle-exit", "-1"],
        &["--idle-exit", "NaN"],
        &["--idle-exit", "inf"],
    ] {
        let Err(error) = Config::from_args(&args(invalid)) else {
            panic!("accepted {invalid:?}");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{invalid:?}");
        assert_eq!(error.to_string(), "runtime.arguments", "{invalid:?}");
    }
}
