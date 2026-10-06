//! The live call detector: Rustvani's VAD (and optional Smart Turn) over the call's audio.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use rustvani::frames::{AudioRawData, Frame, FrameDirection, FrameInner, FrameKind, SystemFrame};
use rustvani::pipeline::{PipelineParams, PipelineTask};
use rustvani::turn::SmartTurnConfig;
use rustvani::vad::{VadBackend, VadParams, VadProcessor};
use tokio::sync::{mpsc, Mutex};

use crate::types::CallSettings;

#[cfg(test)]
mod tests;

const SAMPLE_RATE: u32 = 16_000;
/// While our own speech plays, only louder input may start a turn.
const PLAYING_MIN_VOLUME: f32 = 0.8;

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
    stop_secs: f32,
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
        let stop_secs = vad_stop_secs(settings, vad_stop_override());
        let active = Self::run(
            settings,
            stop_secs,
            false,
            tx.clone(),
            generation.clone(),
            0,
        )?;
        Ok((
            Self {
                active: Mutex::new(active),
                settings: settings.clone(),
                stop_secs,
                generation,
                tx,
            },
            rx,
        ))
    }

    /// Starts one detector pipeline whose events count only while `current` is the generation.
    fn run(
        settings: &CallSettings,
        stop_secs: f32,
        playing: bool,
        tx: mpsc::Sender<CallFrame>,
        generation: Arc<AtomicU64>,
        current: u64,
    ) -> Result<DetectorRun, String> {
        let task = Arc::new(PipelineTask::new(
            vec![vad(settings, stop_secs, playing)?.into_processor()],
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
                if let Some(event) = call_frame(&frame.inner) {
                    let _ = tx.send(event).await;
                }
            })
        });
        let runner = task.clone();
        let worker = tokio::spawn(async move {
            let _ = runner.run(rustvani::system_clock(), None).await;
        });
        Ok(DetectorRun {
            task,
            worker,
            playing,
            #[cfg(test)]
            min_volume: min_volume(settings, playing),
        })
    }

    /// Rustvani currently takes VadParams at construction. Swap its configured
    /// detector at a playback boundary; the input source remains live throughout.
    pub async fn listening_bar(&self, playing: bool) {
        let mut active = self.active.lock().await;
        if active.playing == playing {
            return;
        }
        self.replace(&mut active, playing);
    }

    /// Forget whatever the detector was hearing. A turn closed because the audio stopped leaves
    /// Silero mid-speech; a fresh detector opens the next turn as soon as the person is heard again.
    pub async fn reset(&self) {
        let mut active = self.active.lock().await;
        let playing = active.playing;
        self.replace(&mut active, playing);
    }

    fn replace(&self, active: &mut DetectorRun, playing: bool) {
        let next_generation = self.generation.load(Ordering::Acquire) + 1;
        let Ok(next) = Self::run(
            &self.settings,
            self.stop_secs,
            playing,
            self.tx.clone(),
            self.generation.clone(),
            next_generation,
        ) else {
            return;
        };
        self.generation.store(next_generation, Ordering::Release);
        let old = std::mem::replace(active, next);
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
                Frame::input_audio_raw(AudioRawData::new(pcm, SAMPLE_RATE, 1)),
                FrameDirection::Downstream,
            )
            .await
            .map_err(|error| error.to_string())
    }
}

/// The VAD's stop in timer mode before the speech timeout runs (Python's fixed 0.2 s).
const TIMER_VAD_STOP_SECS: f32 = 0.2;

/// How long a pause Rustvani's VAD waits before it reports the end of speech. In smart-turn mode it
/// is the floor before smart-turn is asked. In timer mode Python's VAD reports the pause and its
/// speech timeout then runs on top, so the single Rustvani stop folds both. `VOICE_VAD_STOP_SECS`
/// replaces the VAD's part in either mode, as in Python (`pipeline/call.py`, `vad_analyzer`).
pub fn vad_stop_secs(settings: &CallSettings, configured: Option<f32>) -> f32 {
    if settings.turn_end_mode == "smart_turn" {
        configured.unwrap_or(settings.smart_turn_min_silence)
    } else {
        configured.unwrap_or(TIMER_VAD_STOP_SECS) + settings.user_speech_timeout
    }
}

fn vad_stop_override() -> Option<f32> {
    parse_vad_stop(std::env::var("VOICE_VAD_STOP_SECS").ok().as_deref())
}

/// A readable, non-negative number of seconds; anything else leaves the mode's own stop in place.
fn parse_vad_stop(value: Option<&str>) -> Option<f32> {
    value
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
}

impl Drop for CallDetector {
    fn drop(&mut self) {
        self.active.get_mut().worker.abort();
    }
}

fn vad(settings: &CallSettings, stop_secs: f32, playing: bool) -> Result<VadProcessor, String> {
    let smart_turn = settings.turn_end_mode == "smart_turn";
    let vad = VadProcessor::new(
        SAMPLE_RATE,
        VadParams {
            confidence: settings.vad_confidence,
            min_volume: min_volume(settings, playing),
            start_secs: settings.vad_start_secs,
            stop_secs,
        },
        VadBackend::Ort,
    )
    .map_err(|error| error.to_string())?;
    if !smart_turn {
        return Ok(vad);
    }
    let config = SmartTurnConfig {
        stop_secs: settings.smart_turn_max_silence,
        ..SmartTurnConfig::default()
    };
    vad.with_smart_turn(Some(&config))
        .map_err(|error| error.to_string())
}

fn min_volume(settings: &CallSettings, playing: bool) -> f32 {
    if playing {
        settings.vad_min_volume.max(PLAYING_MIN_VOLUME)
    } else {
        settings.vad_min_volume
    }
}

fn call_frame(inner: &FrameInner) -> Option<CallFrame> {
    match inner {
        FrameInner::System(SystemFrame::InputAudioRaw(data)) => {
            Some(CallFrame::Audio(data.audio.to_vec()))
        }
        FrameInner::System(SystemFrame::VADUserStartedSpeaking { .. }) => Some(CallFrame::Started),
        FrameInner::System(SystemFrame::VADUserStoppedSpeaking { stop_secs, .. }) => {
            Some(CallFrame::Stopped {
                stop_secs: *stop_secs,
            })
        }
        _ => None,
    }
}
