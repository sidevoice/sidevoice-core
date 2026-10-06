//! Playback dispatch: each call's queue of utterances, held, interrupted or sent one at a time,
//! and the journal rows that show how far each reply got.
use serde_json::json;

use super::latency::{latency_now_micros, LatencyEvent};
use super::{Inner, Room};

/// Most utterances one call may have queued.
pub(super) const MAX_PENDING: usize = 16;

impl Room {
    pub fn speech_current(&self, sid: &str, uid: &str, revision: u64) -> bool {
        let inner = self.inner.lock().expect("room lock");
        inner.browsers.get(sid).is_some_and(|browser| {
            browser.revision == revision
                && !browser.speaking
                && browser.active.as_deref() == Some(uid)
        }) && inner
            .utterances
            .client_entry(uid, sid)
            .is_some_and(|(entry_revision, status)| {
                *entry_revision == revision
                    && !matches!(
                        status.as_str(),
                        "interrupted" | "failed" | "playback_finished"
                    )
            })
    }
}

impl Inner {
    /// Show on a reply's journal row the furthest status any call reached with it; the reason
    /// is kept only when that status is the one that just `changed`.
    pub(super) fn sync_row(&mut self, row_id: &str, changed: &str, reason: Option<&str>) {
        let best = self
            .utterances
            .row_status(row_id)
            .unwrap_or(changed)
            .to_owned();
        if let Some(row) = self.journal.find_mut(row_id) {
            row.reason = if best == changed {
                reason.map(str::to_owned)
            } else {
                None
            };
            row.status = best;
        }
    }

    /// Send a call the next utterance it should play, skipping those it can no longer play.
    pub(super) fn dispatch_client(&mut self, sid: &str) {
        loop {
            let Some(uid) = self
                .browsers
                .get(sid)
                .and_then(|browser| browser.next_to_play().cloned())
            else {
                return;
            };
            let entry = self.utterances.get(&uid).and_then(|record| {
                record
                    .clients
                    .get(sid)
                    .map(|(revision, status)| (record.row_id.clone(), *revision, status.clone()))
            });
            let Some((row_id, revision, status)) = entry else {
                self.skip_next(sid);
                continue;
            };
            if status == "waiting_for_turn" {
                return;
            }
            if status != "queued" {
                self.skip_next(sid);
                continue;
            }
            let Some(row) = self.journal.find(&row_id) else {
                self.skip_next(sid);
                continue;
            };
            let (thread, text, language, reply_revision) = (
                row.thread.clone(),
                row.text.clone(),
                row.language.clone(),
                row.revision,
            );
            let browser = self.browsers.get_mut(sid).expect("browser present");
            if browser.revision != revision || !browser.is_on(&thread) {
                browser.pending.pop_front();
                self.utterances.set_client_status(&uid, sid, "interrupted");
                if self.utterances.get(&uid).is_some_and(|r| !r.is_replay()) {
                    self.sync_row(&row_id, "interrupted", Some("focus_changed"));
                }
                self.utterances.retire_finished_replays();
                continue;
            }
            let event = json!({"type":"voice-speech","data":{"session_id":sid,"utterance_id":uid,"revision":revision,"reply_revision":reply_revision,"thread_id":thread,"text":text,"language":language,"history_id":row_id}});
            if !browser.offer(event) {
                return;
            }
            browser.pending.pop_front();
            browser.active = Some(uid.clone());
            self.mark_latency(
                sid,
                &thread,
                reply_revision,
                Some(&uid),
                LatencyEvent::SynthesisStarted,
                latency_now_micros(),
            );
            return;
        }
    }
    fn skip_next(&mut self, sid: &str) {
        self.browsers
            .get_mut(sid)
            .expect("browser present")
            .pending
            .pop_front();
    }

    /// Stop everything a call had to play, marking `reason` on the rows nobody plays any more.
    pub(super) fn interrupt_client(&mut self, sid: &str, reason: &str) {
        for row_id in self.utterances.interrupt_client(sid) {
            if let Some(row) = self.journal.find_mut(&row_id) {
                if row.status != "playback_finished" {
                    row.status = "interrupted".into();
                    row.reason = Some(reason.into());
                }
            }
        }
        if let Some(browser) = self.browsers.get_mut(sid) {
            browser.pending.clear();
            browser.active = None;
        }
        self.utterances.retire_finished_replays();
    }

    /// The user started turn `revision`: hold the call's queue until the turn ends, putting the
    /// utterance it was about to play back at the front.
    pub(super) fn hold_client(&mut self, sid: &str, revision: u64) {
        let active = self.browsers.get_mut(sid).and_then(|b| b.active.take());
        let held = self.utterances.hold_client(sid, revision);
        if let Some(uid) = active {
            if held.waiting.iter().any(|(id, _)| id == &uid) {
                if let Some(browser) = self.browsers.get_mut(sid) {
                    browser.pending.push_front(uid);
                }
            }
        }
        for row_id in held.waiting.into_iter().filter_map(|(_, row_id)| row_id) {
            self.sync_row(&row_id, "waiting_for_turn", Some("user_speaking"));
        }
        for row_id in held.interrupted {
            self.sync_row(&row_id, "interrupted", Some("newer_turn"));
        }
        self.utterances.retire_finished_replays();
    }
}
