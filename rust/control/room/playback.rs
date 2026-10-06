//! Playback dispatch: each call's queue of utterances, held, interrupted or sent one at a time,
//! and the journal rows that show how far each reply got.
use std::time::{Duration, Instant};

use serde_json::json;

use super::latency::{latency_now_micros, LatencyEvent};
use super::{Inner, Room};

/// Most utterances one call may have queued.
pub(super) const MAX_PENDING: usize = 16;
/// A reply handed to a call is not waited on for ever: 60 s plus the text at 6 characters/s.
const PLAYBACK_BASE_SECONDS: f64 = 60.0;
const PLAYBACK_CHARS_PER_SECOND: f64 = 6.0;

pub(super) fn playback_bound(text: &str) -> Duration {
    Duration::from_secs_f64(
        PLAYBACK_BASE_SECONDS + text.chars().count() as f64 / PLAYBACK_CHARS_PER_SECOND,
    )
}

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
            if !matches!(status.as_str(), "queued" | "waiting_for_pause") {
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
            // The person just stopped speaking: the reply waits out the pause before it starts.
            let now = Instant::now();
            if browser.quiet_until.is_some_and(|until| until > now) {
                if status == "queued" {
                    self.set_status_synced(&uid, sid, "waiting_for_pause", Some("quiet_grace"));
                }
                return;
            }
            if status == "waiting_for_pause" {
                self.set_status_synced(&uid, sid, "queued", None);
            }
            let (replay, requested) = self
                .utterances
                .get(&uid)
                .map_or((false, false), |r| (r.is_replay(), r.requested));
            let mut data = json!({"session_id":sid,"utterance_id":uid,"revision":revision,"reply_revision":reply_revision,"thread_id":thread,"text":text,"language":language,"history_id":row_id});
            if replay {
                data["replay"] = json!(true);
            }
            if requested {
                data["requested"] = json!(true);
            }
            let browser = self.browsers.get_mut(sid).expect("browser present");
            if !browser.offer(json!({"type":"voice-speech","data":data})) {
                return;
            }
            browser.pending.pop_front();
            browser.active = Some(uid.clone());
            browser.playback_watch = Some((uid.clone(), now + playback_bound(&text)));
            if let Some(record) = self.utterances.get_mut(&uid) {
                record.dispatched = true;
            }
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
    /// One call's entry for an utterance moves; the journal row follows unless it is a replay.
    fn set_status_synced(&mut self, uid: &str, sid: &str, status: &str, reason: Option<&str>) {
        self.utterances.set_client_status(uid, sid, status);
        if let Some(row_id) = self
            .utterances
            .get(uid)
            .filter(|r| !r.is_replay())
            .map(|r| r.row_id.clone())
        {
            self.sync_row(&row_id, status, reason);
        }
    }

    /// A killed tab, a network gone mid-playback or a lost receipt would otherwise hold the head
    /// of the queue for ever: past its bound the reply is marked unconfirmed and the queue moves on.
    pub(super) fn expire_playback(&mut self, sid: &str, now: Instant) {
        let Some(browser) = self.browsers.get_mut(sid) else {
            return;
        };
        let Some((uid, deadline)) = browser.playback_watch.clone() else {
            return;
        };
        if browser.active.as_deref() != Some(uid.as_str()) {
            browser.playback_watch = None;
            return;
        }
        if now < deadline {
            return;
        }
        browser.playback_watch = None;
        browser.active = None;
        let open = self
            .utterances
            .client_entry(&uid, sid)
            .is_some_and(|(_, status)| {
                !matches!(
                    status.as_str(),
                    "failed" | "playback_finished" | "interrupted"
                )
            });
        if open {
            self.set_status_synced(&uid, sid, "failed", Some("unconfirmed"));
            self.latency.set_reply_status(sid, &uid, "failed");
        }
        self.utterances.retire_finished_replays();
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
            browser.playback_watch = None;
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
            self.sync_row(&row_id, "interrupted", Some("user_interrupted"));
        }
        for original in held.heard_replays {
            self.utterances
                .replay_heard(&original, sid, "playing", "interrupted");
        }
        self.utterances.retire_finished_replays();
    }
}
