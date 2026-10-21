use clap::Parser;
use sidevoice_core::runtime::{self, Cli, Config};

#[tokio::main]
async fn main() {
    let config = match Config::from_cli(Cli::parse()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("sidevoice-core: {error}");
            std::process::exit(2);
        }
    };
    std::process::exit(runtime::run(config).await);
}
