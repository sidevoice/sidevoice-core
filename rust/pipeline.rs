//! Rustvani's detector stays behind this application boundary.

use std::{collections::HashSet, path::Path, sync::Arc};

use rustvani::frames::{AudioRawData, Frame, FrameDirection, FrameInner, FrameKind, SystemFrame};
use rustvani::pipeline::{PipelineParams, PipelineTask};
use rustvani::turn::{SmartTurnAnalyzer, SmartTurnConfig};
use rustvani::vad::{SileroVadOrt, VadBackend, VadParams, VadProcessor};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::types::CallSettings;

/// Audio and detector events share the Rustvani downstream order. The call owner
/// buffers the complete turn from these events and never handles Rustvani types.
pub enum CallFrame {
    Audio(Vec<u8>),
    Started,
    Stopped { stop_secs: f32 },
}

pub struct CallDetector {
    task: Arc<PipelineTask>,
    worker: tokio::task::JoinHandle<()>,
}

impl CallDetector {
    pub fn start(settings: &CallSettings) -> Result<(Self, mpsc::Receiver<CallFrame>), String> {
        let stop_secs = if settings.turn_end_mode == "smart_turn" {
            settings.smart_turn_min_silence
        } else {
            settings.user_speech_timeout
        };
        let vad = VadProcessor::new(
            16_000,
            VadParams {
                confidence: settings.vad_confidence,
                min_volume: settings.vad_min_volume,
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
        let (tx, rx) = mpsc::channel(128);
        task.add_on_frame_reached_downstream(move |frame| {
            let tx = tx.clone();
            Box::pin(async move {
                let event = match frame.inner {
                    FrameInner::System(SystemFrame::InputAudioRaw(data)) => {
                        Some(CallFrame::Audio(data.audio.to_vec()))
                    }
                    FrameInner::System(SystemFrame::VADUserStartedSpeaking { .. }) => {
                        Some(CallFrame::Started)
                    }
                    FrameInner::System(SystemFrame::VADUserStoppedSpeaking { stop_secs, .. }) => {
                        Some(CallFrame::Stopped { stop_secs })
                    }
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
        Ok((Self { task, worker }, rx))
    }

    pub async fn feed(&self, pcm: Vec<u8>) -> Result<(), String> {
        if pcm.len() < 2 || pcm.len() % 2 != 0 {
            return Err("invalid_pcm".to_owned());
        }
        self.task
            .push_frame(
                Frame::input_audio_raw(AudioRawData::new(pcm, 16_000, 1)),
                FrameDirection::Downstream,
            )
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn close(self) {
        let _ = self.task.push_frame(Frame::cancel(), FrameDirection::Downstream).await;
        self.worker.abort();
    }
}

impl Drop for CallDetector {
    fn drop(&mut self) { self.worker.abort(); }
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
