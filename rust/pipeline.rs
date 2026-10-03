//! Rustvani's detector stays behind this application boundary.

use std::{
    collections::HashSet,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use rustvani::frames::{AudioRawData, Frame, FrameDirection, FrameInner, FrameKind, SystemFrame};
use rustvani::pipeline::{PipelineParams, PipelineTask};
use rustvani::turn::{SmartTurnAnalyzer, SmartTurnConfig};
use rustvani::vad::{SileroVadOrt, VadBackend, VadParams, VadProcessor};
use serde::Serialize;
use tokio::sync::{mpsc, Mutex};

use crate::types::CallSettings;

/// Audio and detector events share the Rustvani downstream order. The call owner
/// buffers the complete turn from these events and never handles Rustvani types.
pub enum CallFrame {
    Audio(Vec<u8>),
    Started,
    Stopped { stop_secs: f32 },
}

pub struct CallDetector {
    active: Mutex<DetectorRun>,
    settings: CallSettings,
    generation: Arc<AtomicU64>,
    tx: mpsc::Sender<CallFrame>,
}

struct DetectorRun {
    task: Arc<PipelineTask>,
    worker: tokio::task::JoinHandle<()>,
    playing: bool,
    #[cfg(test)]
    min_volume: f32,
}

impl CallDetector {
    pub fn start(settings: &CallSettings) -> Result<(Self, mpsc::Receiver<CallFrame>), String> {
        let (tx, rx) = mpsc::channel(128);
        let generation = Arc::new(AtomicU64::new(0));
        let active = Self::run(settings, false, tx.clone(), generation.clone(), 0)?;
        Ok((
            Self {
                active: Mutex::new(active),
                settings: settings.clone(),
                generation,
                tx,
            },
            rx,
        ))
    }

    fn run(
        settings: &CallSettings,
        playing: bool,
        tx: mpsc::Sender<CallFrame>,
        generation: Arc<AtomicU64>,
        current: u64,
    ) -> Result<DetectorRun, String> {
        let stop_secs = if settings.turn_end_mode == "smart_turn" {
            settings.smart_turn_min_silence
        } else {
            settings.user_speech_timeout
        };
        let min_volume = if playing {
            settings.vad_min_volume.max(0.8)
        } else {
            settings.vad_min_volume
        };
        let vad = VadProcessor::new(
            16_000,
            VadParams {
                confidence: settings.vad_confidence,
                min_volume,
                start_secs: settings.vad_start_secs,
                stop_secs,
            },
            VadBackend::Ort,
        )
        .map_err(|error| error.to_string())?;
        let vad = if settings.turn_end_mode == "smart_turn" {
            let config = SmartTurnConfig {
                stop_secs: settings.smart_turn_max_silence,
                ..SmartTurnConfig::default()
            };
            vad.with_smart_turn(Some(&config))
                .map_err(|error| error.to_string())?
        } else {
            vad
        };
        let task = Arc::new(PipelineTask::new(
            vec![vad.into_processor()],
            PipelineParams::default(),
        ));
        task.set_downstream_filter(HashSet::from([
            FrameKind::InputAudioRaw,
            FrameKind::VADUserStartedSpeaking,
            FrameKind::VADUserStoppedSpeaking,
        ]));
        task.add_on_frame_reached_downstream(move |frame| {
            let tx = tx.clone();
            let generation = generation.clone();
            Box::pin(async move {
                if generation.load(Ordering::Acquire) != current {
                    return;
                }
                let event = match frame.inner {
                    FrameInner::System(SystemFrame::InputAudioRaw(data)) => {
                        Some(CallFrame::Audio(data.audio.to_vec()))
                    }
                    FrameInner::System(SystemFrame::VADUserStartedSpeaking { .. }) => {
                        Some(CallFrame::Started)
                    }
                    FrameInner::System(SystemFrame::VADUserStoppedSpeaking {
                        stop_secs, ..
                    }) => Some(CallFrame::Stopped { stop_secs }),
                    _ => None,
                };
                if let Some(event) = event {
                    let _ = tx.send(event).await;
                }
            })
        });
        let runner = task.clone();
        let worker = tokio::spawn(async move {
            let _ = runner.run(rustvani::system_clock(), None).await;
        });
        Ok(DetectorRun { task, worker, playing, #[cfg(test)] min_volume })
    }

    /// Rustvani currently takes VadParams at construction. Swap its configured
    /// detector at a playback boundary; the input source remains live throughout.
    pub async fn listening_bar(&self, playing: bool) {
        let mut active = self.active.lock().await;
        if active.playing == playing {
            return;
        }
        let next_generation = self.generation.load(Ordering::Acquire) + 1;
        let Ok(next) = Self::run(
            &self.settings,
            playing,
            self.tx.clone(),
            self.generation.clone(),
            next_generation,
        ) else {
            return;
        };
        self.generation.store(next_generation, Ordering::Release);
        let old = std::mem::replace(&mut *active, next);
        old.worker.abort();
    }

    pub async fn feed(&self, pcm: Vec<u8>) -> Result<(), String> {
        if pcm.len() < 2 || !pcm.len().is_multiple_of(2) {
            return Err("invalid_pcm".to_owned());
        }
        self.active
            .lock()
            .await
            .task
            .push_frame(
                Frame::input_audio_raw(AudioRawData::new(pcm, 16_000, 1)),
                FrameDirection::Downstream,
            )
            .await
            .map_err(|error| error.to_string())
    }
}

impl Drop for CallDetector {
    fn drop(&mut self) {
        self.active.get_mut().worker.abort();
    }
}

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

/// The baseline complete-turn speech gate is independent of the start detector.
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
        let rms = (frame
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let sample = i16::from_le_bytes([pair[0], pair[1]]) as f64 / 32768.0;
                sample * sample
            })
            .sum::<f64>()
            / 512.0)
            .sqrt();
        let confidence = detector.infer_async(frame).await?;
        peak = peak.max(confidence);
        if confidence >= 0.5 && rms >= 0.001 {
            speech_frames += 1;
        }
    }
    Ok(speech_frames >= 3 && peak >= 0.9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn playback_bar_uses_rustvani_threshold_and_restores_settings() {
        let mut settings = crate::models::default_settings(None, None);
        settings.turn_end_mode = "timer".into();
        let (detector, _events) = CallDetector::start(&settings).unwrap();
        assert_eq!(detector.active.lock().await.min_volume, settings.vad_min_volume);
        detector.listening_bar(true).await;
        let active = detector.active.lock().await;
        assert!(active.playing);
        assert_eq!(active.min_volume, 0.8);
        drop(active);
        detector.listening_bar(false).await;
        let active = detector.active.lock().await;
        assert!(!active.playing);
        assert_eq!(active.min_volume, settings.vad_min_volume);
    }
}
