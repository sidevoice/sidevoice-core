use std::path::PathBuf;

use serde_json::json;
use sidevoice_core::messages::{render, LocalizedMessage};
use sidevoice_core::pipeline::probe_detectors;
use sidevoice_core::runtime::{self, Config};

#[tokio::main]
async fn main() {
    let Some(flags): Option<Vec<String>> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_str().map(str::to_owned))
        .collect()
    else {
        eprintln!("{}", render(&LocalizedMessage::new("runtime.arguments"), &runtime::system_language()));
        std::process::exit(2);
    };
    if flags.iter().any(|flag| flag == "--help" || flag == "-h") {
        println!("{}", render(&LocalizedMessage::new("runtime.help"), &runtime::system_language()));
        return;
    }
    if flags.first().map(String::as_str) == Some("--self-test") {
        if flags.len() != 3 {
            eprintln!("{}", json!({"error_key": "rust_core_t0_usage"}));
            std::process::exit(2);
        }
        let wav = PathBuf::from(&flags[1]);
        let assets = PathBuf::from(&flags[2]);
        match probe_detectors(
            &wav,
            &assets.join("silero.onnx"),
            &assets.join("smart_turn_weights.bin.gz"),
        )
        .await
        {
            Ok(readout) => println!("{}", json!({"detectors": readout})),
            Err(error) => {
                eprintln!(
                    "{}",
                    json!({"error_key": "rust_core_t0_detector_failed", "detail": error})
                );
                std::process::exit(1);
            }
        }
        return;
    }
    let config = match Config::from_args(&flags) {
        Ok(config) => config,
        Err(_) => {
            eprintln!("{}", render(&LocalizedMessage::new("runtime.arguments"), &runtime::system_language()));
            std::process::exit(2);
        }
    };
    std::process::exit(runtime::run(config).await);
}
