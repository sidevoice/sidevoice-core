use clap::Parser;
use sidevoice_core::runtime::{self, Cli, Command, Config};

// The log does not exist before the configuration: a bad one is told on stderr (`core.stderr.log`), by its kind only, since
// its text can hold a path. A command's answer is the one line a person ran it for.
#[allow(
    clippy::disallowed_macros,
    reason = "no subscriber before the configuration; a command prints its answer"
)]
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let command = cli.command;
    let config = match Config::from_cli(cli) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("sidevoice-core: invalid configuration ({:?})", error.kind());
            std::process::exit(2);
        }
    };
    if let Some(Command::CrashReports { action }) = command {
        match runtime::crash_reports(&config, action) {
            Ok(state) => println!("{state}"),
            Err(error) => {
                eprintln!("sidevoice-core: crash reports ({:?})", error.kind());
                std::process::exit(1);
            }
        }
        return;
    }
    if let Err(status) = runtime::init_logging(&config) {
        std::process::exit(status);
    }
    std::process::exit(runtime::run(config).await);
}
