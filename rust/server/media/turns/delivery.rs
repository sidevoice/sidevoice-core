//! Live transcripts reach the room once their merge window closes: consecutive
//! transcripts on the same focus are joined into one input.

use std::time::{Duration, Instant};

use crate::control::room::{latency_now_micros, LatencyEvent, VoiceTurn};

use super::super::recognition::Recognition;
use super::{events, TurnOwner};

pub(super) struct PendingTurn {
    pub(super) turn: VoiceTurn,
    text: String,
    deadline: Instant,
    transcribed_at: u64,
}

impl TurnOwner {
    pub(in crate::server) fn deadline(&self) -> Option<Instant> {
        self.pending
            .as_ref()
            // A resumed segment keeps the first transcript open through recognition.
            .filter(|_| self.speaking.is_none() && self.active.is_none() && self.queue.is_empty())
            .map(|p| p.deadline)
    }

    pub(in crate::server) async fn expired(&mut self) {
        if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            if let Some(pending) = self.pending.take() {
                self.deliver(pending.turn, pending.text, pending.transcribed_at)
                    .await;
            }
        }
    }

    pub(super) async fn live_result(
        &mut self,
        turn: VoiceTurn,
        result: Recognition,
        closed: u64,
        transcribed: u64,
    ) {
        if self.room.turn_cancelled(&self.session, turn.revision) {
            self.room.finish_turn(&self.session, turn.revision);
            return;
        }
        let recognition_ms = transcribed.saturating_sub(closed) as f64 / 1000.0;
        self.mark(&turn, LatencyEvent::Transcript, transcribed);
        self.duration(&turn, "recognition_ms", recognition_ms);
        self.duration(&turn, "request_to_transcript_ms", recognition_ms);
        let text = match result {
            Ok(text) => text.unwrap_or_default(),
            Err(error) => {
                self.report_failure(error, false).await;
                String::new()
            }
        };
        let current = self.room.snapshot(Some(&self.session));
        let revision = current["room"]["revision"].as_u64().unwrap_or(0);
        let same_focus = current["binding"]["thread_id"].as_str() == turn.thread_id.as_deref();
        if text.is_empty() {
            self.abandon(&turn).await;
            return;
        }
        if revision > turn.revision && same_focus {
            self.hold(turn, text, transcribed).await;
            return;
        }
        if revision > turn.revision || self.settings.merge_window_secs <= 0.0 {
            self.deliver(turn, text, transcribed).await;
            return;
        }
        let Some((turn, text)) = self.merge_pending(turn, text, transcribed).await else {
            return;
        };
        self.pending = Some(PendingTurn {
            turn,
            text,
            deadline: self.merge_deadline(),
            transcribed_at: transcribed,
        });
    }

    /// The room moved on while this turn was recognized: hold it for the next
    /// transcript on the same focus instead of delivering it alone.
    async fn hold(&mut self, turn: VoiceTurn, text: String, transcribed: u64) {
        let Some((turn, text)) = self.merge_pending(turn, text, transcribed).await else {
            return;
        };
        self.announce(events::merged(&turn, &text)).await;
        self.pending = Some(PendingTurn {
            turn,
            text,
            deadline: Instant::now(),
            transcribed_at: transcribed,
        });
    }

    /// Joins the transcript onto a pending one on the same focus, or delivers a
    /// pending one on another focus. Returns the transcript when it was not joined.
    async fn merge_pending(
        &mut self,
        turn: VoiceTurn,
        text: String,
        transcribed: u64,
    ) -> Option<(VoiceTurn, String)> {
        let Some(previous) = self.pending.take() else {
            return Some((turn, text));
        };
        if previous.turn.thread_id == turn.thread_id {
            self.room.finish_turn(&self.session, previous.turn.revision);
            self.pending = Some(PendingTurn {
                turn,
                text: format!("{} {}", previous.text, text),
                deadline: self.merge_deadline(),
                transcribed_at: transcribed,
            });
            return None;
        }
        self.deliver(previous.turn, previous.text, previous.transcribed_at)
            .await;
        Some((turn, text))
    }

    fn merge_deadline(&self) -> Instant {
        Instant::now() + Duration::from_secs_f32(self.settings.merge_window_secs)
    }

    async fn deliver(&mut self, turn: VoiceTurn, text: String, transcribed_at: u64) {
        if self.room.turn_cancelled(&self.session, turn.revision) {
            self.room.finish_turn(&self.session, turn.revision);
            return;
        }
        self.announce(events::finished(&turn, &text)).await;
        let _ = self.room.queue_voice_input(&turn, &text);
        let delivered = latency_now_micros();
        self.mark(&turn, LatencyEvent::TranscriptDelivered, delivered);
        self.duration(
            &turn,
            "transcript_to_delivery_ms",
            delivered.saturating_sub(transcribed_at) as f64 / 1000.0,
        );
        self.room.finish_turn(&self.session, turn.revision);
    }
}
