use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use super::support::args;
use crate::runtime::{Command, CommandError};

#[test]
fn help_wins_over_any_other_argument() {
    for flags in [
        &["--help"][..],
        &["--port", "invalid", "-h"],
        &["--self-test", "--help"],
    ] {
        assert!(
            matches!(Command::parse(&args(flags)), Ok(Command::Help)),
            "{flags:?}"
        );
    }
}

#[test]
fn self_test_takes_exactly_a_wav_and_an_asset_directory() {
    let Ok(Command::SelfTest { wav, assets }) =
        Command::parse(&args(&["--self-test", "probe.wav", "models"]))
    else {
        panic!("self-test not parsed");
    };
    assert_eq!(wav, PathBuf::from("probe.wav"));
    assert_eq!(assets, PathBuf::from("models"));
    for flags in [&["--self-test"][..], &["--self-test", "a", "b", "c"]] {
        assert_eq!(
            Command::parse(&args(flags)).err(),
            Some(CommandError::SelfTestUsage)
        );
    }
}

#[test]
fn serve_flags_parse_into_a_configuration() {
    let Ok(Command::Serve(config)) = Command::parse(&args(&["--port", "9001"])) else {
        panic!("serve not parsed");
    };
    assert_eq!(config.port, 9001);
    assert_eq!(
        Command::parse(&args(&["--port"])).err(),
        Some(CommandError::Arguments)
    );
}

#[test]
fn non_utf8_arguments_are_invalid_before_help() {
    let args = [
        OsString::from("--help"),
        OsString::from_vec(vec![0x66, 0x6f, 0x80]),
    ];
    assert_eq!(
        Command::from_args_os(args).err(),
        Some(CommandError::Arguments)
    );
}
