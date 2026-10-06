//! Recognition runs one job at a time, in arrival order, behind a bounded queue.

use crate::control::room::{latency_now_micros, VoiceTurn};

use super::super::{
    audio::GATE_RATE,
    recognition::{Recognition, Recognizer, SttFailure},
};
use super::TurnOwner;

pub(super) const MAX_RECOGNITION_QUEUE: usize = 8;

pub(super) enum RecognitionJob {
    /// A turn spoken during the call, captured at the detector's rate.
    Live {
        turn: VoiceTurn,
        pcm: Vec<u8>,
        closed: u64,
    },
    /// Audio the browser buffered while it was disconnected.
    Offline {
        target: VoiceTurn,
        row_id: String,
        pcm: Vec<u8>,
        rate: u32,
        truncated: bool,
        time: Option<u64>,
    },
}

impl RecognitionJob {
    pub(super) fn live_turn(&self) -> Option<&VoiceTurn> {
        match self {
            Self::Live { turn, .. } => Some(turn),
            Self::Offline { .. } => None,
        }
    }

    fn take_audio(&mut self) -> (Vec<u8>, u32) {
        match self {
            Self::Live { pcm, .. } => (std::mem::take(pcm), GATE_RATE),
            Self::Offline { pcm, rate, .. } => (std::mem::take(pcm), *rate),
        }
    }
}

pub(in crate::server) struct RecognitionDone {
    job: RecognitionJob,
    result: Recognition,
    transcribed: u64,
}

impl TurnOwner {
    pub(super) async fn enqueue(&mut self, job: RecognitionJob) {
        if self.queue.len() + usize::from(self.active.is_some()) >= MAX_RECOGNITION_QUEUE {
            if let Some(turn) = job.live_turn() {
                self.abandon(turn).await;
            }
            self.report_failure(SttFailure::Provider, false).await;
            return;
        }
        self.queue.push_back(job);
        self.start_next();
    }

    pub(super) fn start_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(mut job) = self.queue.pop_front() else {
            return;
        };
        self.active_turn = job.live_turn().cloned();
        let media = self.media.clone();
        let settings = self.settings.clone();
        let session = self.session.clone();
        let events = self.events.clone();
        let dir = self.dir.clone();
        let finished = self.finished_tx.clone();
        self.active = Some(tokio::spawn(async move {
            let (pcm, rate) = job.take_audio();
            let recognizer = Recognizer {
                transcripts: media.transcripts(),
                settings: &settings,
                session: &session,
                events: &events,
                dir: &dir,
            };
            let result = recognizer.recognize(pcm, rate).await;
            let _ = finished
                .send(RecognitionDone {
                    job,
                    result,
                    transcribed: latency_now_micros(),
                })
                .await;
        }));
    }

    /// Takes one finished recognition; a live result for a cancelled turn is stale.
    pub(in crate::server) async fn result(&mut self, done: RecognitionDone) {
        if let Some(turn) = done.job.live_turn() {
            if self.active_turn.as_ref().map(|active| active.revision) != Some(turn.revision) {
                return;
            }
        }
        if let Some(active) = self.active.take() {
            let _ = active.await;
        }
        self.active_turn = None;
        let RecognitionDone {
            job,
            result,
            transcribed,
        } = done;
        match job {
            RecognitionJob::Live { turn, closed, .. } => {
                self.live_result(turn, result, closed, transcribed).await;
            }
            RecognitionJob::Offline {
                target,
                row_id,
                truncated,
                time,
                ..
            } => {
                self.offline_result(&target, &row_id, truncated, time, result)
                    .await;
            }
        }
        self.start_next();
    }
}
