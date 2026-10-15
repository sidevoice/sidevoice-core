//! Replies on their way to the calls listening: each is sent once to every call on its conversation, whose voice
//! module decides whether and when it plays, and the journal row shows how far it got from what the calls report.
use serde_json::{json, Value};

use super::error::RoomError;
use super::latency::{latency_now_micros, LatencyEvent};
use super::utterances::{ClientEntry, End, UtteranceRecord};
use super::{Inner, Room};

/// What a call may report about a reply it was sent.
const REPORTS: [&str; 5] = ["playing", "heard", "interrupted", "unplayed", "failed"];
/// Why a reply stopped short, as a call may say.
const REASONS: [&str; 9] = [
    "user_interrupted",
    "newer_turn",
    "focus_changed",
    "call_ended",
    "unheard",
    "agents_muted",
    "voice_off",
    "connector_disconnected",
    "heard_elsewhere",
];

impl Room {
    /// Physical playback and diagnostic stop cause. Semantic hearing arrives in reply-hearing.
    pub fn playback(
        &self,
        sid: &str,
        uid: &str,
        status: &str,
        reason: Option<&str>,
        heard_chars: Option<u64>,
        timings: &Value,
    ) -> Result<(), RoomError> {
        if status.is_empty() {
            return Err(RoomError::new(400, "room.receipt_invalid"));
        }
        if !REPORTS.contains(&status) {
            return Ok(());
        }
        let reason = reason.filter(|reason| REASONS.contains(reason));
        let stale = || RoomError::new(409, "room.stale_utterance");
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if !inner.browsers.contains(sid) {
            return Err(stale());
        }
        let Some(record) = inner.utterances.get(uid) else {
            return Err(stale());
        };
        let length = inner
            .journal
            .find(&record.row_id)
            .map(|row| row.text.chars().count() as u64);
        if heard_chars.is_some_and(|heard| length.is_some_and(|length| heard > length)) {
            return Err(RoomError::new(400, "room.receipt_invalid"));
        }
        let record = inner.utterances.get_mut(uid).ok_or_else(stale)?;
        let entry = record.clients.get_mut(sid).ok_or_else(stale)?;
        let next = match status {
            "heard" => "playback_finished",
            "interrupted" | "unplayed" => "interrupted",
            "failed" => "failed",
            _ => "playing",
        };
        if !matches!(entry.1.as_str(), "queued" | "playing") {
            // A delayed physical receipt may improve history diagnostics, never semantic hearing.
            if entry.1 == "interrupted" && matches!(status, "heard" | "interrupted") {
                if status == "heard" {
                    entry.1 = next.into();
                }
                if let Some(end) = record.ends.get_mut(sid) {
                    end.heard_chars = match (end.heard_chars, heard_chars) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                }
                if !record.is_replay() {
                    let row_id = record.row_id.clone();
                    inner.sync_row(&row_id);
                }
            }
            return Ok(());
        }
        entry.1 = next.into();
        record.ends.insert(
            sid.to_owned(),
            End {
                reason: match status {
                    "heard" | "playing" => None,
                    "interrupted" => Some(reason.unwrap_or("user_interrupted").into()),
                    "unplayed" => Some(reason.unwrap_or("newer_turn").into()),
                    "failed" => Some("playback_failed".into()),
                    _ => None,
                },
                heard_chars: if status == "heard" {
                    length
                } else {
                    heard_chars
                },
            },
        );
        let row_id = record.row_id.clone();
        if !record.is_replay() {
            inner.sync_row(&row_id);
        }
        inner.latency.set_reply_status(sid, uid, next);
        if status == "playing" {
            if let Some((thread, reply_revision)) = inner.latency.reply_turn(sid, uid) {
                inner.mark_latency(
                    sid,
                    &thread,
                    reply_revision,
                    Some(uid),
                    LatencyEvent::PlayingReceipt,
                    latency_now_micros(),
                );
            }
        }
        inner.utterances.retire_finished_replays();
        drop(guard);
        self.latency_browser(sid, uid, timings);
        Ok(())
    }
}

impl Inner {
    /// Show physical playback state in history without inferring semantic hearing from it.
    pub(super) fn sync_row(&mut self, row_id: &str) {
        let Some((status, reason, _)) = self
            .utterances
            .original_of_row(row_id)
            .and_then(|(_, record)| record.outcome())
        else {
            return;
        };
        if let Some(row) = self.journal.find_mut(row_id) {
            row.status = status.to_owned();
            row.reason = reason.map(str::to_owned);
        }
    }

    /// Send reply `uid` to call `sid` as text. A call that is away gets nothing: when it comes back, what was
    /// published meanwhile is marked unheard.
    pub(super) fn send_reply(&mut self, sid: &str, uid: &str) {
        let Some(browser) = self.browsers.get(sid).filter(|b| b.parked.is_none()) else {
            return;
        };
        let Some(record) = self.utterances.get(uid) else {
            return;
        };
        let Some((revision, _)) = record.clients.get(sid) else {
            return;
        };
        let Some(row) = self.journal.find(&record.row_id) else {
            return;
        };
        let mut data = json!({"session_id":sid,"utterance_id":uid,"revision":revision,"reply_revision":row.revision,"epoch":row.epoch,
            "thread_id":row.thread,"text":row.text,"language":row.language,"history_id":row.id});
        if record.is_replay() {
            data["replay"] = json!(true);
        }
        if record.requested {
            data["requested"] = json!(true);
        }
        let (thread, reply_revision) = (row.thread.clone(), row.revision);
        if !browser.offer(json!({"type":"voice-reply","data":data})) {
            // The call's channel is full: the reply never reaches it, so it ends there unheard.
            self.set_status_synced(uid, sid, "interrupted", Some("unheard"));
            self.latency.set_reply_status(sid, uid, "failed");
            return;
        }
        if let Some(record) = self.utterances.get_mut(uid) {
            record.sent.insert(sid.to_owned());
        }
        self.mark_latency(
            sid,
            &thread,
            reply_revision,
            Some(uid),
            LatencyEvent::ReplyDispatched,
            latency_now_micros(),
        );
    }

    /// One call's entry for an utterance moves; the journal row follows unless it is a replay.
    fn set_status_synced(&mut self, uid: &str, sid: &str, status: &str, reason: Option<&str>) {
        let end = End {
            reason: reason.map(str::to_owned),
            heard_chars: None,
        };
        self.utterances.set_client_status(uid, sid, status, end);
        if let Some(row_id) = self
            .utterances
            .get(uid)
            .filter(|r| !r.is_replay())
            .map(|r| r.row_id.clone())
        {
            self.sync_row(&row_id);
        }
    }

    /// What a returning call never received stays written and is marked unheard: it is not played late. A reply
    /// published while it was away was never sent to it; `unreceived` are those sent whose frame never reached the
    /// page. One the page did receive may still be playing there, and its report counts.
    pub(super) fn drop_unheard(&mut self, sid: &str, _unreceived: &[String]) {
        let dropped: Vec<String> = self
            .utterances
            .iter()
            .filter(|(_, record)| {
                record
                    .clients
                    .get(sid)
                    .is_some_and(|(_, status)| matches!(status.as_str(), "queued" | "playing"))
            })
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in dropped {
            self.set_status_synced(&uid, sid, "interrupted", Some("unheard"));
            self.latency.set_reply_status(sid, &uid, "failed");
        }
        self.utterances.retire_finished_replays();
    }

    /// Withdraws from call `sid` the replies `stale` picks among those it has not finished, for `reason`
    /// (`newer_turn`, `focus_changed`). Ones it was sent are named in one `voice-reply-withdrawn`: the call cancels
    /// them, and its playback reports say what became of each (how far each was heard, or that it never played).
    /// Ones it was never sent, or a withdrawal it cannot be handed, end here, unheard for `reason`.
    pub(super) fn withdraw(
        &mut self,
        sid: &str,
        reason: &str,
        stale: impl Fn(&UtteranceRecord, &ClientEntry) -> bool,
    ) {
        let (sent, unsent): (Vec<_>, Vec<_>) = self
            .utterances
            .iter()
            .filter(|(_, record)| {
                record.clients.get(sid).is_some_and(|entry| {
                    matches!(entry.1.as_str(), "queued" | "playing") && stale(record, entry)
                })
            })
            .map(|(uid, record)| (uid.clone(), record.sent.contains(sid)))
            .partition(|(_, sent)| *sent);
        let sent: Vec<String> = sent.into_iter().map(|(uid, _)| uid).collect();
        let mut ended: Vec<String> = unsent.into_iter().map(|(uid, _)| uid).collect();
        if sent.is_empty() && ended.is_empty() {
            return;
        }
        self.count_cancel(sid, reason);
        if !sent.is_empty() {
            let told = self.browsers.get(sid).is_some_and(|browser| {
                browser.offer(json!({"type":"voice-reply-withdrawn","data":{
                    "session_id":sid,"utterance_ids":sent,"reason":reason}}))
            });
            if !told {
                ended.extend(sent);
            }
        }
        for uid in ended {
            self.set_status_synced(&uid, sid, "interrupted", Some(reason));
            self.latency.set_reply_status(sid, &uid, "interrupted");
        }
        self.utterances.retire_finished_replays();
    }

    /// Stop everything a call had to play, for `reason`.
    pub(super) fn interrupt_client(&mut self, sid: &str, reason: &str) {
        // A halt that interrupted nothing is not a cancellation: counting it would make every turn look like one.
        if self
            .utterances
            .has_client_status(sid, &["queued", "playing"])
        {
            self.count_cancel(sid, reason);
        }
        for row_id in self.utterances.interrupt_client(sid, reason) {
            self.sync_row(&row_id);
        }
        self.utterances.retire_finished_replays();
    }

    fn count_cancel(&self, sid: &str, reason: &str) {
        if let Some(telemetry) = crate::control::telemetry::shared() {
            let browser = self.browsers.get(sid);
            let thread = browser
                .and_then(|b| b.target.as_ref())
                .map(|t| t.thread.as_str());
            telemetry.cancelled(sid, reason, thread, browser.map(|b| b.revision));
        }
    }
}
