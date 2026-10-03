use std::path::PathBuf;

use serde_json::json;
use sidevoice_core::pipeline::probe_detectors;
use sidevoice_core::runtime::{self, Config};

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|arg| arg.to_str()) == Some("--self-test") {
        if args.len() != 4 {
            eprintln!("{}", json!({"error_key": "rust_core_t0_usage"}));
            std::process::exit(2);
        }
        let wav = PathBuf::from(&args[2]);
        let assets = PathBuf::from(&args[3]);
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
    let flags: Vec<_> = args
        .iter()
        .skip(1)
        .filter_map(|arg| arg.to_str().map(str::to_owned))
        .collect();
    let config = match Config::from_args(&flags) {
        Ok(config) => config,
        Err(_) => {
            eprintln!("{}", json!({"error_key": "runtime.arguments"}));
            std::process::exit(2);
        }
    };
    std::process::exit(runtime::run(config).await);
}
