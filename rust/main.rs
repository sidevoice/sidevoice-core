use std::path::PathBuf;

use opus::{Application, Channels, Decoder, Encoder};
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
        eprintln!(
            "{}",
            render(
                &LocalizedMessage::new("runtime.arguments"),
                &runtime::system_language()
            )
        );
        std::process::exit(2);
    };
    if flags.iter().any(|flag| flag == "--help" || flag == "-h") {
        println!(
            "{}",
            render(
                &LocalizedMessage::new("runtime.help"),
                &runtime::system_language()
            )
        );
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
            Ok(readout) => match probe_codec() {
                Ok(samples) => println!(
                    "{}",
                    json!({"detectors": readout, "opus_decoded_samples": samples})
                ),
                Err(error) => {
                    eprintln!(
                        "{}",
                        json!({"error_key": "rust_core_t0_detector_failed", "detail": error})
                    );
                    std::process::exit(1);
                }
            },
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
            eprintln!(
                "{}",
                render(
                    &LocalizedMessage::new("runtime.arguments"),
                    &runtime::system_language()
                )
            );
            std::process::exit(2);
        }
    };
    std::process::exit(runtime::run(config).await);
}

fn probe_codec() -> Result<usize, String> {
    let mut encoder = Encoder::new(16_000, Channels::Mono, Application::Audio)
        .map_err(|error| error.to_string())?;
    let mut decoder = Decoder::new(16_000, Channels::Mono).map_err(|error| error.to_string())?;
    let pcm: Vec<i16> = (0..320)
        .map(|sample| ((sample as f32 * 0.08).sin() * 4_000.0) as i16)
        .collect();
    let mut packet = [0_u8; 1500];
    let size = encoder
        .encode(&pcm, &mut packet)
        .map_err(|error| error.to_string())?;
    let mut decoded = [0_i16; 320];
    let samples = decoder
        .decode(&packet[..size], &mut decoded, false)
        .map_err(|error| error.to_string())?;
    if samples != 320 || decoded.iter().all(|sample| *sample == 0) {
        return Err("opus_roundtrip_empty".to_owned());
    }
    Ok(samples)
}
