//! The finite, machine-readable detector and codec self-test for hosted CI.

use std::path::Path;

use opus::{Application, Channels, Decoder, Encoder};
use serde_json::{json, Value};

use crate::pipeline::probe_detectors;

/// Probe the detectors on `wav` with the models staged in `assets`, then the Opus codec.
/// The error is a detail for the `rust_core_t0_detector_failed` report.
pub async fn self_test(wav: &Path, assets: &Path) -> Result<Value, String> {
    let readout = probe_detectors(
        wav,
        &assets.join("silero.onnx"),
        &assets.join("smart_turn_weights.bin.gz"),
    )
    .await?;
    let samples = probe_codec()?;
    Ok(json!({"detectors": readout, "opus_decoded_samples": samples}))
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
