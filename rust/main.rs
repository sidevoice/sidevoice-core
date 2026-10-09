use sidevoice_core::messages::{render, LocalizedMessage};
use sidevoice_core::runtime::{self, Command, CommandError};

#[tokio::main]
async fn main() {
    match Command::from_args_os(std::env::args_os().skip(1)) {
        Ok(Command::Help) => println!("{}", localized("runtime.help")),
        Ok(Command::Serve(config)) => std::process::exit(runtime::run(config).await),
        Err(CommandError::Arguments) => {
            eprintln!("{}", localized("runtime.arguments"));
            std::process::exit(2);
        }
    }
}

fn localized(key: &str) -> String {
    render(&LocalizedMessage::new(key), &runtime::system_language())
}
