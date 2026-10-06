//! Replays: speaking an earlier reply again to one call without touching its history row.
use std::collections::HashMap;

use serde_json::{json, Value};

use super::error::RoomError;
use super::playback::MAX_PENDING;
use super::util::millis;
use super::utterances::UtteranceRecord;
use super::Room;

pub(super) const MAX_REPLAY_RECORDS: usize = 16;
/// Most missed replies one returning call is handed; recency is what really bounds it.
pub(super) const MAX_MISSED: usize = 8;

/// A reply a returning call never heard through, as `Room::missed_replies` finds it.
pub struct MissedReply {
    pub utterance_id: String,
    pub history_id: String,
    pub text: String,
    pub language: Option<String>,
    /// Already handed out once: a paid render of it is never bought again.
    pub rendered: bool,
}

impl Room {
    pub fn replay_source(
        &self,
        sid: &str,
        history_id: &str,
    ) -> Result<(String, Option<String>), RoomError> {
        let inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        let Some((_, record)) = inner.utterances.original_of_row(history_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let Some(row) = inner.journal.find(&record.row_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        if !browser.is_on(&row.thread) {
            return Err(RoomError::new(404, "room.replay_missing"));
        }
        Ok((row.text.clone(), row.language.clone()))
    }

    pub fn replay_one(&self, sid: &str, history_id: &str, uid: &str) -> Result<Value, RoomError> {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if inner.utterances.replay_count() >= MAX_REPLAY_RECORDS {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let Some((original, _)) = inner.utterances.original_of_row(history_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let original = original.clone();
        let Some(row) = inner.journal.find(history_id) else {
            return Err(RoomError::new(404, "room.replay_missing"));
        };
        let thread = row.thread.clone();
        let Some(browser) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if !browser.is_on(&thread) {
            return Err(RoomError::new(404, "room.replay_missing"));
        }
        // Leave one queue slot for the next live reply, even during a replay burst.
        if browser.pending.len() >= MAX_PENDING - 1 {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let event = json!({"type":"voice-replay","data":{"session_id":sid,"thread_id":thread,
            "replies":[{"utterance_id":uid,"history_id":history_id}],"skipped":[]}});
        if !browser.offer(event) {
            return Err(RoomError::new(429, "room.replay_full"));
        }
        let revision = browser.revision;
        browser.pending.push_front(uid.to_owned());
        inner.utterances.insert(
            uid,
            UtteranceRecord {
                row_id: history_id.to_owned(),
                clients: HashMap::from([(sid.to_owned(), (revision, "queued".to_owned()))]),
                replay_of: Some(original),
                requested: true,
                ..Default::default()
            },
        );
        inner.dispatch_client(sid);
        Ok(json!({"utterance_id":uid,"history_id":history_id}))
    }
    /// The replies on this call's conversation it never heard through, oldest first and at most
    /// `MAX_MISSED`, published in the last `seconds`. `sessions` are the IDs this tab used before:
    /// naming one can only take replies away from the answer, never add somebody else's.
    pub fn missed_replies(&self, sid: &str, seconds: f64, sessions: &[String]) -> Vec<MissedReply> {
        if seconds.is_nan() || seconds <= 0.0 {
            return Vec::new();
        }
        let inner = self.inner.lock().expect("room lock");
        let Some(thread) = inner
            .browsers
            .get(sid)
            .and_then(|browser| browser.bound_target())
            .map(|target| target.thread.as_str())
        else {
            return Vec::new();
        };
        let floor = millis().saturating_sub((seconds * 1000.0) as u64);
        let mut missed: Vec<(u64, MissedReply)> = inner
            .utterances
            .iter()
            .filter(|(_, record)| !record.is_replay())
            // Its own entry: still queued or playing, finished, or failed here, is not missed.
            // Cut by a move to another conversation (interrupted, not heard) is.
            .filter(|(_, record)| {
                record.clients.get(sid).is_none_or(|(_, status)| {
                    status == "interrupted" && !record.heard.contains(sid)
                })
            })
            .filter(|(_, record)| {
                !record.heard.contains(sid)
                    && !sessions
                        .iter()
                        .any(|session| record.heard.contains(session))
            })
            .filter_map(|(uid, record)| {
                let row = inner.journal.find(&record.row_id)?;
                (row.thread == thread && row.time >= floor).then(|| {
                    (
                        row.seq,
                        MissedReply {
                            utterance_id: uid.clone(),
                            history_id: row.id.clone(),
                            text: row.text.clone(),
                            language: row.language.clone(),
                            rendered: record.dispatched && !record.parked,
                        },
                    )
                })
            })
            .collect();
        missed.sort_by_key(|(seq, _)| *seq);
        let skip = missed.len().saturating_sub(MAX_MISSED);
        missed
            .into_iter()
            .skip(skip)
            .map(|(_, reply)| reply)
            .collect()
    }

    /// Queue catch-ups of missed replies for one call, behind what it already has, and tell it
    /// first so the bubbles say they are being repeated. `queued` pairs each catch-up's utterance
    /// ID with the reply it repeats; `skipped` are history IDs whose audio the room no longer has.
    pub fn replay_missed(
        &self,
        sid: &str,
        queued: &[(String, String)],
        skipped: &[String],
    ) -> Value {
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        let Some((thread, revision)) = inner.browsers.get(sid).and_then(|browser| {
            browser
                .bound_target()
                .map(|target| (target.thread.clone(), browser.revision))
        }) else {
            return json!({"replayed":[],"skipped":[]});
        };
        let mut replayed = Vec::new();
        for (uid, original) in queued {
            if inner.utterances.get(uid).is_some() {
                continue;
            }
            let Some(row_id) = inner
                .utterances
                .get(original)
                .filter(|record| !record.is_replay())
                .map(|record| record.row_id.clone())
            else {
                continue;
            };
            let Some(browser) = inner.browsers.get_mut(sid) else {
                break;
            };
            // Leave one queue slot for the next live reply, as a requested replay does.
            if inner.utterances.replay_count() >= MAX_REPLAY_RECORDS
                || browser.pending.len() >= MAX_PENDING - 1
            {
                break;
            }
            browser.pending.push_back(uid.clone());
            inner.utterances.insert(
                uid,
                UtteranceRecord {
                    row_id: row_id.clone(),
                    clients: HashMap::from([(sid.to_owned(), (revision, "queued".to_owned()))]),
                    replay_of: Some(original.clone()),
                    ..Default::default()
                },
            );
            replayed.push(json!({"utterance_id":uid,"history_id":row_id}));
        }
        let skipped: Vec<Value> = skipped
            .iter()
            .map(|history_id| json!({"history_id":history_id,"reason":"audio_gone"}))
            .collect();
        if !replayed.is_empty() || !skipped.is_empty() {
            if let Some(browser) = inner.browsers.get(sid) {
                browser.notify(json!({"type":"voice-replay","data":{
                    "session_id":sid,"thread_id":thread,"replies":replayed,"skipped":skipped}}));
            }
        }
        inner.dispatch_client(sid);
        json!({"replayed":replayed,"skipped":skipped})
    }
    pub fn has_replay(&self, uid: &str) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .utterances
            .get(uid)
            .is_some_and(|record| record.is_replay())
    }
}
