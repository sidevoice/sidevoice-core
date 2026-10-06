//! What the process was asked to do, from its command line.

use std::ffi::OsString;
use std::path::PathBuf;

use super::Config;

pub enum Command {
    Help,
    SelfTest { wav: PathBuf, assets: PathBuf },
    Serve(Config),
}

#[derive(Debug, PartialEq)]
pub enum CommandError {
    /// The arguments are not valid; rendered as `runtime.arguments`.
    Arguments,
    /// `--self-test` without exactly a WAV file and an asset directory.
    SelfTestUsage,
}

impl Command {
    /// Parse the arguments after the program name. Any non-UTF-8 argument is invalid.
    pub fn from_args_os(args: impl IntoIterator<Item = OsString>) -> Result<Self, CommandError> {
        let flags: Option<Vec<String>> =
            args.into_iter().map(|arg| arg.into_string().ok()).collect();
        Self::parse(&flags.ok_or(CommandError::Arguments)?)
    }

    pub fn parse(flags: &[String]) -> Result<Self, CommandError> {
        if flags.iter().any(|flag| flag == "--help" || flag == "-h") {
            return Ok(Self::Help);
        }
        if flags.first().map(String::as_str) == Some("--self-test") {
            let [_, wav, assets] = flags else {
                return Err(CommandError::SelfTestUsage);
            };
            return Ok(Self::SelfTest {
                wav: PathBuf::from(wav),
                assets: PathBuf::from(assets),
            });
        }
        Config::from_args(flags)
            .map(Self::Serve)
            .map_err(|_| CommandError::Arguments)
    }
}
