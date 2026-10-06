//! PCM conversions for recognition: the 16 kHz speech-gate copy and WAV framing.

use rustvani::audio_process::resamplers::{ResamplerQuality, StreamResampler};

/// The speech gate and the live detector both run at this rate.
pub(super) const GATE_RATE: u32 = 16_000;

/// Returns 16-bit little-endian mono PCM at `GATE_RATE`, copying when already there.
pub(super) fn to_gate_rate(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    if sample_rate == GATE_RATE {
        return pcm.to_vec();
    }
    let mut resampler = StreamResampler::new(sample_rate, GATE_RATE, ResamplerQuality::Quick);
    let samples: Vec<f32> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair) as f32 / 32768.0)
        .collect();
    let mut output = resampler.process(&samples);
    output.extend(resampler.flush());
    output
        .into_iter()
        .flat_map(|sample| ((sample * 32767.0).clamp(-32768.0, 32767.0) as i16).to_le_bytes())
        .collect()
}

pub(in crate::server) fn wav(pcm: &[u8], sample_rate: u32) -> Option<Vec<u8>> {
    if pcm.is_empty() || !pcm.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::new();
    let cursor = std::io::Cursor::new(&mut bytes);
    let mut writer = hound::WavWriter::new(
        cursor,
        hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .ok()?;
    for sample in pcm.as_chunks::<2>().0 {
        writer
            .write_sample(i16::from_le_bytes([sample[0], sample[1]]))
            .ok()?;
    }
    writer.finalize().ok()?;
    Some(bytes)
}
