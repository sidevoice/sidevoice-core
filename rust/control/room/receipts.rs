//! Playback receipts: a browser reporting what happened to an utterance it was given.
use serde_json::{json, Value};

use super::error::RoomError;
use super::latency::{latency_now_micros, LatencyEvent};
use super::Room;

const RECEIPTS: [&str; 6] = [
    "playing",
    "failed",
    "playback_finished",
    "skipped",
    "cancelled_unplayed",
    "cancelled_playing",
];

impl Room {
    pub fn receipt(
        &self,
        sid: &str,
        uid: &str,
        revision: u64,
        status: &str,
    ) -> Result<Value, RoomError> {
        if !RECEIPTS.contains(&status) {
            return Err(RoomError::new(400, "room.receipt_invalid"));
        }
        let stale = || RoomError::new(409, "room.stale_utterance");
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let Some(browser) = inner.browsers.get(sid) else {
            return Err(stale());
        };
        // Skips and cancellations may name an older revision than the call's current one.
        let special = matches!(
            status,
            "skipped" | "cancelled_unplayed" | "cancelled_playing"
        );
        if !special && browser.revision != revision {
            return Err(stale());
        }
        let active = browser.active.as_deref() == Some(uid);
        if matches!(status, "playing" | "failed" | "playback_finished")
            && (!active || browser.speaking)
        {
            return Err(stale());
        }
        let Some(record) = inner.utterances.get_mut(uid) else {
            return Err(stale());
        };
        let Some(entry) = record.clients.get_mut(sid) else {
            return Err(stale());
        };
        if (special && revision > entry.0) || (!special && entry.0 != revision) {
            return Err(stale());
        }
        if matches!(
            entry.1.as_str(),
            "failed" | "playback_finished" | "interrupted"
        ) {
            if special {
                return Ok(json!({"status":status}));
            }
            return Err(stale());
        }
        if status == "cancelled_unplayed" && revision < entry.0 {
            return Ok(json!({"status":status}));
        }
        let next = match status {
            "skipped" | "cancelled_playing" => "interrupted",
            "cancelled_unplayed" => "waiting_for_turn",
            other => other,
        };
        let previous = std::mem::replace(&mut entry.1, next.into());
        if matches!(
            status,
            "playback_finished" | "skipped" | "cancelled_playing"
        ) {
            record.heard.insert(sid.to_owned());
        }
        let catch_up_of = record.replay_of.clone();
        let original_row = (!record.is_replay()).then(|| record.row_id.clone());
        if let Some(row_id) = &original_row {
            let best = record.best_status().unwrap_or(next).to_owned();
            if let Some(row) = inner.journal.find_mut(row_id) {
                row.status = best;
                row.reason = reason(status).map(str::to_owned);
            }
            inner.track_unheard(row_id);
        }
        if let Some(original) = catch_up_of {
            inner
                .utterances
                .replay_heard(&original, sid, &previous, next);
        }
        if status != "playing" {
            let browser = inner.browsers.get_mut(sid).expect("browser present");
            if active {
                browser.active = None;
            }
            if status == "cancelled_unplayed" {
                if !browser.pending.iter().any(|item| item == uid) {
                    browser.pending.push_front(uid.into());
                }
            } else {
                browser.pending.retain(|item| item != uid);
            }
            inner.dispatch_client(sid);
        }
        inner.latency.set_reply_status(sid, uid, status);
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
        Ok(json!({"status":status}))
    }
}

/// Why a reply's row stopped where it did, as the receipt that stopped it tells.
fn reason(receipt: &str) -> Option<&'static str> {
    match receipt {
        "skipped" => Some("user_skipped"),
        "cancelled_playing" => Some("user_interrupted"),
        "cancelled_unplayed" => Some("user_speaking"),
        "failed" => Some("playback_failed"),
        _ => None,
    }
}
