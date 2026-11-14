use clap::Parser;
use sidevoice_core::runtime::{self, Cli, Config};

// The log does not exist before the configuration: a bad one is told on stderr (`core.stderr.log`), by its kind only, since
// its text can hold a path.
#[allow(
    clippy::disallowed_macros,
    reason = "no subscriber before the configuration"
)]
#[tokio::main]
async fn main() {
    let config = match Config::from_cli(Cli::parse()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("sidevoice-core: invalid configuration ({:?})", error.kind());
            std::process::exit(2);
        }
    };
    if let Err(status) = runtime::init_logging(&config) {
        std::process::exit(status);
    }
    std::process::exit(runtime::run(config).await);
}
