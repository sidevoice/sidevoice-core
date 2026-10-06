//! The baseline complete-turn speech gate is independent of the start detector.

use rustvani::vad::SileroVadOrt;

/// A fresh Silero state prevents one accepted turn from blessing later noise.
pub async fn has_speech(pcm: &[u8]) -> Result<bool, String> {
    if pcm.is_empty() {
        return Ok(false);
    }
    let detector = SileroVadOrt::new(16_000)?;
    let mut speech_frames = 0;
    let mut peak = 0.0_f32;
    for samples in pcm.chunks(1024) {
        let mut frame = samples.to_vec();
        frame.resize(1024, 0);
        let rms = rms(&frame);
        let confidence = detector.infer_async(frame).await?;
        peak = peak.max(confidence);
        if confidence >= 0.5 && rms >= 0.001 {
            speech_frames += 1;
        }
    }
    Ok(speech_frames >= 3 && peak >= 0.9)
}

/// Root mean square of one 512-sample frame of 16-bit little-endian PCM.
fn rms(frame: &[u8]) -> f64 {
    (frame
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let sample = i16::from_le_bytes([pair[0], pair[1]]) as f64 / 32768.0;
            sample * sample
        })
        .sum::<f64>()
        / 512.0)
        .sqrt()
}
