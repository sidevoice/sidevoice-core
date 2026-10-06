//! The call's turn owner: buffers speech into turns, recognizes them in order and
//! delivers each transcript to the room.

mod catchup;
mod delivery;
mod events;
mod queue;

#[cfg(test)]
mod tests;

use std::{collections::VecDeque, sync::Arc};

use serde_json::Value;
use tokio::sync::mpsc;

use self::{
    catchup::Catchup,
    delivery::PendingTurn,
    queue::{RecognitionDone, RecognitionJob},
};
use super::call::CallMedia;
use crate::{
    control::room::{latency_now_micros, LatencyEvent, Room, VoiceTurn},
    pipeline::CallFrame,
    storage::PrivateDir,
    types::CallSettings,
};

const MAX_TURN_BYTES: usize = 16_000 * 2 * 60;
const PRE_ROLL_BYTES: usize = 16_000 * 2;

pub(in crate::server) struct TurnOwner {
    media: Arc<CallMedia>,
    room: Arc<Room>,
    settings: CallSettings,
    session: String,
    events: mpsc::Sender<Value>,
    dir: PrivateDir,
    recent: VecDeque<u8>,
    speaking: Option<(VoiceTurn, Vec<u8>)>,
    active: Option<tokio::task::JoinHandle<()>>,
    active_turn: Option<VoiceTurn>,
    queue: VecDeque<RecognitionJob>,
    catchup: Option<Catchup>,
    catchups: u64,
    pending: Option<PendingTurn>,
    pub(in crate::server) finished: mpsc::Receiver<RecognitionDone>,
    finished_tx: mpsc::Sender<RecognitionDone>,
}

impl TurnOwner {
    pub(in crate::server) fn new(
        media: Arc<CallMedia>,
        room: Arc<Room>,
        settings: CallSettings,
        session: String,
        events: mpsc::Sender<Value>,
        dir: PrivateDir,
    ) -> Self {
        let (finished_tx, finished) = mpsc::channel(16);
        Self {
            media,
            room,
            settings,
            session,
            events,
            dir,
            recent: VecDeque::new(),
            speaking: None,
            active: None,
            active_turn: None,
            queue: VecDeque::new(),
            catchup: None,
            catchups: 0,
            pending: None,
            finished,
            finished_tx,
        }
    }

    pub(in crate::server) async fn frame(&mut self, frame: CallFrame) {
        match frame {
            CallFrame::Audio(bytes) => self.buffer(bytes),
            CallFrame::Started => self.start_speaking().await,
            CallFrame::Stopped { stop_secs } => self.stop_speaking(stop_secs).await,
        }
    }

    /// Speech goes to the open turn; silence keeps only the pre-roll before the next one.
    fn buffer(&mut self, bytes: Vec<u8>) {
        if let Some((_, pcm)) = &mut self.speaking {
            if pcm.len() + bytes.len() <= MAX_TURN_BYTES {
                pcm.extend_from_slice(&bytes);
            }
        } else {
            self.recent.extend(bytes);
            while self.recent.len() > PRE_ROLL_BYTES {
                self.recent.pop_front();
            }
        }
    }

    async fn start_speaking(&mut self) {
        if self.speaking.is_some() {
            return;
        }
        self.media.listening_bar(false).await;
        if let Ok(turn) = self.room.begin_turn(&self.session) {
            self.announce(events::started(&turn)).await;
            let pcm = self.recent.drain(..).collect();
            self.speaking = Some((turn, pcm));
        }
    }

    async fn stop_speaking(&mut self, stop_secs: f32) {
        let Some((turn, pcm)) = self.speaking.take() else {
            return;
        };
        let closed = latency_now_micros();
        let speech_end = closed.saturating_sub((stop_secs as f64 * 1_000_000.0) as u64);
        self.mark(&turn, LatencyEvent::SpeechEnd, speech_end);
        self.mark(&turn, LatencyEvent::TurnClosed, closed);
        self.duration(&turn, "endpoint_silence_ms", stop_secs as f64 * 1000.0);
        self.duration(&turn, "audio_ms", pcm.len() as f64 / 32.0);
        self.enqueue(RecognitionJob::Live { turn, pcm, closed })
            .await;
    }

    pub(in crate::server) async fn close(&mut self) {
        self.media.close();
        if let Some(active) = self.active.take() {
            active.abort();
            let _ = active.await;
        }
        if let Some(turn) = self.active_turn.take() {
            self.room.finish_turn(&self.session, turn.revision);
        }
        for job in self.queue.drain(..) {
            if let Some(turn) = job.live_turn() {
                self.room.finish_turn(&self.session, turn.revision);
            }
        }
        self.catchup = None;
        if let Some((turn, _)) = self.speaking.take() {
            self.room.finish_turn(&self.session, turn.revision);
        }
        if let Some(pending) = self.pending.take() {
            self.room.finish_turn(&self.session, pending.turn.revision);
        }
    }

    pub(in crate::server) async fn cancel(&mut self, revision: u64) {
        if self
            .speaking
            .as_ref()
            .is_some_and(|(turn, _)| turn.revision == revision)
        {
            self.speaking = None;
            self.recent.clear();
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.turn.revision == revision)
        {
            self.pending = None;
        }
        self.queue
            .retain(|job| job.live_turn().is_none_or(|turn| turn.revision != revision));
        if self.active_turn.as_ref().map(|turn| turn.revision) == Some(revision) {
            if let Some(active) = self.active.take() {
                active.abort();
                let _ = active.await;
            }
            self.active_turn = None;
            self.start_next();
        }
        self.room.finish_turn(&self.session, revision);
    }

    /// Closes the open turn at a focus change and reopens one on the new focus.
    pub(in crate::server) async fn focus_changed(&mut self) {
        let Some((turn, pcm)) = self.speaking.take() else {
            return;
        };
        let closed = latency_now_micros();
        self.mark(&turn, LatencyEvent::TurnClosed, closed);
        self.enqueue(RecognitionJob::Live { turn, pcm, closed })
            .await;
        if let Ok(turn) = self.room.begin_turn(&self.session) {
            self.announce(events::started(&turn)).await;
            self.speaking = Some((turn, Vec::new()));
        }
    }

    fn mark(&self, turn: &VoiceTurn, event: LatencyEvent, at_micros: u64) {
        if let Some(thread) = turn.thread_id.as_deref() {
            self.room
                .latency_mark(&self.session, thread, turn.revision, None, event, at_micros);
        }
    }

    fn duration(&self, turn: &VoiceTurn, name: &str, milliseconds: f64) {
        if let Some(thread) = turn.thread_id.as_deref() {
            self.room.latency_duration(
                &self.session,
                thread,
                turn.revision,
                None,
                name,
                milliseconds,
            );
        }
    }
}
