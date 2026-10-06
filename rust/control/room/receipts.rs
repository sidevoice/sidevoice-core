//! Playback receipts: a browser reporting what happened to an utterance it was given.
use serde_json::{json, Value};

use super::error::RoomError;
use super::latency::{latency_now_micros, mark_latency, status_latency_reply, LatencyEvent};
use super::playback::{dispatch_client, status_rank};
use super::replay::retire_terminal_replays;
use super::Room;

impl Room {
    pub fn receipt(
        &self,
        sid: &str,
        uid: &str,
        revision: u64,
        status: &str,
    ) -> Result<Value, RoomError> {
        if ![
            "playing",
            "failed",
            "playback_finished",
            "skipped",
            "cancelled_unplayed",
            "cancelled_playing",
        ]
        .contains(&status)
        {
            return Err(RoomError::new(400, "room.receipt_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get(sid) else {
            return Err(RoomError::new(409, "room.stale_utterance"));
        };
        let special = matches!(
            status,
            "skipped" | "cancelled_unplayed" | "cancelled_playing"
        );
        if !special && browser.revision != revision {
            return Err(RoomError::new(409, "room.stale_utterance"));
        }
        let active = browser.active.as_deref() == Some(uid);
        if matches!(status, "playing" | "failed" | "playback_finished")
            && (!active || browser.speaking)
        {
            return Err(RoomError::new(409, "room.stale_utterance"));
        }
        let Some(record) = inner.utterances.get_mut(uid) else {
            return Err(RoomError::new(409, "room.stale_utterance"));
        };
        let Some(entry) = record.clients.get_mut(sid) else {
            return Err(RoomError::new(409, "room.stale_utterance"));
        };
        if (special && revision > entry.0) || (!special && entry.0 != revision) {
            return Err(RoomError::new(409, "room.stale_utterance"));
        }
        if matches!(
            entry.1.as_str(),
            "failed" | "playback_finished" | "interrupted"
        ) {
            if special {
                return Ok(json!({"status":status}));
            }
            return Err(RoomError::new(409, "room.stale_utterance"));
        }
        if status == "cancelled_unplayed" && revision < entry.0 {
            return Ok(json!({"status":status}));
        }
        let next = match status {
            "skipped" | "cancelled_playing" => "interrupted",
            "cancelled_unplayed" => "waiting_for_turn",
            other => other,
        };
        entry.1 = next.into();
        let row_id = record.row_id.clone();
        let replay = record.replay_of.is_some();
        let best = record
            .clients
            .values()
            .map(|(_, status)| status.as_str())
            .max_by_key(|status| status_rank(status))
            .unwrap_or(next)
            .to_owned();
        if let Some(row) = inner
            .rows
            .iter_mut()
            .find(|r| r.id == row_id)
            .filter(|_| !replay)
        {
            row.status = best;
            row.reason = match status {
                "skipped" => Some("user_skipped".into()),
                "cancelled_playing" => Some("user_interrupted".into()),
                "cancelled_unplayed" => Some("user_speaking".into()),
                "failed" => Some("playback_failed".into()),
                _ => None,
            };
        }
        let browser = inner.browsers.get_mut(sid).expect("browser present");
        if status != "playing" {
            if active {
                browser.active = None;
            }
            if status == "cancelled_unplayed" && !browser.pending.iter().any(|item| item == uid) {
                browser.pending.push_front(uid.into());
            } else if status != "cancelled_unplayed" {
                browser.pending.retain(|item| item != uid);
            }
            dispatch_client(&mut inner, sid);
        }
        status_latency_reply(&mut inner, sid, uid, status);
        if status == "playing" {
            if let Some((thread, reply_revision)) = inner
                .latency_replies
                .get(sid)
                .and_then(|rows| rows.iter().find(|row| row.utterance_id == uid))
                .map(|row| (row.thread_id.clone(), row.revision))
            {
                mark_latency(
                    &mut inner,
                    sid,
                    &thread,
                    reply_revision,
                    Some(uid),
                    LatencyEvent::PlayingReceipt,
                    latency_now_micros(),
                );
            }
        }
        retire_terminal_replays(&mut inner);
        Ok(json!({"status":status}))
    }
}
