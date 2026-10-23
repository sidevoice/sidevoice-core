use clap::error::ErrorKind;
use clap::CommandFactory;

use super::support::{args, core_endpoint, flag, full_config, root};
use crate::runtime::{Cli, Config};

#[test]
fn every_flag_overrides_its_setting() {
    let config = full_config();
    assert_eq!(config.data_dir, root().join("core"));
    assert_eq!(config.ready_file, root().join("run/ready.json"));
    assert_eq!(config.host, "0.0.0.0");
    assert_eq!(config.port, 9000);
    assert_eq!(config.launch_id, "launch");
    assert_eq!(config.log_file, root().join("log/core.log"));
    assert_eq!(config.idle_exit, 0.0);
}

#[test]
fn unset_paths_derive_from_the_data_directory() {
    let config = Config::from_args(args(&["--data-dir", &flag("core")])).unwrap();
    assert_eq!(config.endpoint.to_string(), core_endpoint());
    assert_eq!(config.ready_file, root().join("core/core.json"));
    assert_eq!(config.log_file, root().join("core.log"));
}

#[test]
fn relative_paths_are_made_absolute() {
    let config = Config::from_args(args(&["--data-dir", "relative/core"])).unwrap();
    assert_eq!(
        config.data_dir,
        std::env::current_dir().unwrap().join("relative/core")
    );
}

#[test]
fn invalid_arguments_are_refused() {
    for invalid in [
        &["--port"][..],
        &["--unknown", "value"],
        &["--port", "not-a-port"],
        &["--port", "70000"],
        &["--idle-exit", "-1"],
        &["--idle-exit", "NaN"],
        &["--idle-exit", "inf"],
        &["stray"],
    ] {
        let Err(error) = Config::from_args(args(invalid)) else {
            panic!("accepted {invalid:?}");
        };
        assert_eq!(error.exit_code(), 2, "{invalid:?}");
    }
}

#[test]
fn help_is_a_successful_display() {
    for flags in [&["--help"][..], &["-h"]] {
        let Err(error) = Config::from_args(args(flags)) else {
            panic!("--help served");
        };
        assert_eq!(error.kind(), ErrorKind::DisplayHelp, "{flags:?}");
        assert_eq!(error.exit_code(), 0, "{flags:?}");
    }
}

#[test]
fn a_flag_may_carry_its_value_after_an_equals_sign() {
    let config = Config::from_args(args(&["--port=9001", "--host=::1"])).unwrap();
    assert_eq!((config.host.as_str(), config.port), ("::1", 9001));
}

// Unix only: a path of arbitrary bytes (not UTF-8) exists on Unix; Windows paths are UTF-16.
#[cfg(unix)]
#[test]
fn non_utf8_paths_are_accepted_but_non_utf8_text_is_refused() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let bytes = || OsString::from_vec(b"/srv/fo\x80".to_vec());
    let path = Config::from_args([OsString::from("--data-dir"), bytes()]).unwrap();
    assert_eq!(path.data_dir.as_os_str(), bytes());
    assert!(Config::from_args([OsString::from("--host"), bytes()]).is_err());
}

#[test]
fn the_environment_variables_are_the_documented_ones() {
    let command = Cli::command();
    let env = |name: &str| {
        command
            .get_arguments()
            .find(|arg| arg.get_id() == name)
            .and_then(|arg| arg.get_env())
            .map(|var| var.to_string_lossy().into_owned())
    };
    assert_eq!(env("host").as_deref(), Some("SIDEVOICE_CORE_HOST"));
    assert_eq!(env("port").as_deref(), Some("SIDEVOICE_CORE_PORT"));
    assert_eq!(env("data_dir").as_deref(), Some("SIDEVOICE_CORE_DATA_DIR"));
    assert_eq!(
        env("idle_exit").as_deref(),
        Some("SIDEVOICE_CORE_IDLE_SECONDS")
    );
    for flag in ["ready_file", "launch_id", "log_file"] {
        assert_eq!(env(flag), None, "{flag}");
    }
}

#[test]
fn the_flag_names_are_unchanged() {
    let names: Vec<_> = Cli::command()
        .get_arguments()
        .filter_map(|arg| arg.get_long().map(str::to_owned))
        .collect();
    assert_eq!(
        names,
        [
            "host",
            "port",
            "data-dir",
            "ready-file",
            "launch-id",
            "log-file",
            "idle-exit"
        ]
    );
}

#[test]
fn the_default_data_directory_is_core_inside_the_shared_one_never_the_current_directory() {
    let Ok(shared) = sidevoice_platform::data_dir::resolve(|name| std::env::var_os(name)) else {
        return; // No data directory to derive from: the core refuses to start without --data-dir.
    };
    let config = Config::from_args(args(&[])).unwrap();
    assert_eq!(config.data_dir, shared.join("core"));
    assert_eq!(config.log_file, shared.join("core.log"));
}
