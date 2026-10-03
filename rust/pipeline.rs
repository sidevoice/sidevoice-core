//! Rustvani's detector stays behind this application boundary.

use std::path::Path;

use rustvani::turn::{SmartTurnAnalyzer, SmartTurnConfig};
use rustvani::vad::SileroVadOrt;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DetectorReadout {
    pub sample_rate: u32,
    pub frames: usize,
    pub max_voice_confidence: f32,
    pub smart_turn_probability: f32,
    pub smart_turn_complete: bool,
}

pub async fn probe_detectors(
    wav_path: &Path,
    silero_path: &Path,
    smart_turn_path: &Path,
) -> Result<DetectorReadout, String> {
    let mut reader = hound::WavReader::open(wav_path).map_err(|e| e.to_string())?;
    let format = reader.spec();
    if format.channels != 1 || format.sample_rate != 16_000 || format.bits_per_sample != 16 {
        return Err("fixture_format_unsupported".into());
    }
    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    if samples.len() < 512 {
        return Err("fixture_too_short".into());
    }

    let silero = SileroVadOrt::from_path(16_000, &silero_path.to_string_lossy())?;
    let config = SmartTurnConfig {
        weights_path: Some(smart_turn_path.to_string_lossy().into_owned()),
        ..SmartTurnConfig::default()
    };
    let mut smart_turn = SmartTurnAnalyzer::new(&config).map_err(|e| e.to_string())?;
    smart_turn.set_sample_rate(16_000);

    let mut max_voice_confidence = 0.0_f32;
    let mut frames = 0;
    let (chunks, _) = samples.as_chunks::<512>();
    for chunk in chunks {
        let mut pcm = Vec::with_capacity(1024);
        for sample in chunk {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let confidence = silero.infer_async(pcm.clone()).await?;
        if !confidence.is_finite() {
            return Err("silero_nonfinite".into());
        }
        max_voice_confidence = max_voice_confidence.max(confidence);
        smart_turn.append_audio(&pcm, confidence >= 0.5);
        frames += 1;
    }
    let (_, metrics) = smart_turn.analyze_end_of_turn();
    let metrics = metrics.ok_or("smart_turn_no_result")?;
    if !metrics.probability.is_finite() {
        return Err("smart_turn_nonfinite".into());
    }

    Ok(DetectorReadout {
        sample_rate: 16_000,
        frames,
        max_voice_confidence,
        smart_turn_probability: metrics.probability,
        smart_turn_complete: metrics.is_complete,
    })
}
