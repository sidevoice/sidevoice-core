use std::path::PathBuf;

use serde_json::json;
use sidevoice_core::pipeline::probe_detectors;

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 4 || args[1].to_str() != Some("--self-test") {
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
}
