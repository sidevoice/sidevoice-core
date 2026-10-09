//! What the process was asked to do, from its command line.

use std::ffi::OsString;

use super::Config;

pub enum Command {
    Help,
    Serve(Config),
}

#[derive(Debug, PartialEq)]
pub enum CommandError {
    /// The arguments are not valid; rendered as `runtime.arguments`.
    Arguments,
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
        Config::from_args(flags)
            .map(Self::Serve)
            .map_err(|_| CommandError::Arguments)
    }
}
