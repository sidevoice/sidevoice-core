//! Playback dispatch: each call's queue of utterances, held, interrupted or sent one at a time.
use serde_json::json;

use super::latency::{latency_now_micros, mark_latency, LatencyEvent};
use super::replay::retire_terminal_replays;
use super::{Inner, Room};

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
            .get(uid)
            .and_then(|record| record.clients.get(sid))
            .is_some_and(|(entry_revision, status)| {
                *entry_revision == revision
                    && !matches!(
                        status.as_str(),
                        "interrupted" | "failed" | "playback_finished"
                    )
            })
    }
}

pub(super) fn status_rank(status: &str) -> u8 {
    match status {
        "failed" => 2,
        "interrupted" => 3,
        "queued" => 4,
        "waiting_for_turn" => 5,
        "playing" => 7,
        "playback_finished" => 8,
        _ => 0,
    }
}
pub(super) fn sync_row(inner: &mut Inner, row_id: &str, changed: &str, reason: Option<&str>) {
    let best = inner
        .utterances
        .values()
        .find(|record| record.row_id == row_id)
        .and_then(|record| {
            record
                .clients
                .values()
                .map(|(_, status)| status.as_str())
                .max_by_key(|status| status_rank(status))
        })
        .unwrap_or(changed)
        .to_owned();
    if let Some(row) = inner.rows.iter_mut().find(|row| row.id == row_id) {
        row.status = best.clone();
        row.reason = if best == changed {
            reason.map(str::to_owned)
        } else {
            None
        };
    }
}

pub(super) fn dispatch_client(inner: &mut Inner, sid: &str) {
    loop {
        let Some(browser) = inner.browsers.get(sid) else {
            return;
        };
        if browser.active.is_some() || browser.speaking {
            return;
        }
        let Some(uid) = browser.pending.front().cloned() else {
            return;
        };
        let entry = inner.utterances.get(&uid).and_then(|record| {
            record
                .clients
                .get(sid)
                .map(|(revision, status)| (record.row_id.clone(), *revision, status.clone()))
        });
        let Some((row_id, revision, status)) = entry else {
            inner
                .browsers
                .get_mut(sid)
                .expect("browser present")
                .pending
                .pop_front();
            continue;
        };
        if status == "waiting_for_turn" {
            return;
        }
        if status != "queued" {
            inner
                .browsers
                .get_mut(sid)
                .expect("browser present")
                .pending
                .pop_front();
            continue;
        }
        let row = inner.rows.iter().find(|r| r.id == row_id).map(|r| {
            (
                r.thread.clone(),
                r.text.clone(),
                r.language.clone(),
                r.revision,
            )
        });
        let Some((thread, text, language, reply_revision)) = row else {
            inner
                .browsers
                .get_mut(sid)
                .expect("browser present")
                .pending
                .pop_front();
            continue;
        };
        let browser = inner.browsers.get_mut(sid).expect("browser present");
        if browser.revision != revision
            || browser.target.as_ref().is_none_or(|t| t.thread != thread)
        {
            browser.pending.pop_front();
            if let Some(record) = inner.utterances.get_mut(&uid) {
                if let Some(entry) = record.clients.get_mut(sid) {
                    entry.1 = "interrupted".into();
                }
            }
            if inner
                .utterances
                .get(&uid)
                .is_some_and(|record| record.replay_of.is_none())
            {
                sync_row(inner, &row_id, "interrupted", Some("focus_changed"));
            }
            retire_terminal_replays(inner);
            continue;
        }
        let event = json!({"type":"voice-speech","data":{"session_id":sid,"utterance_id":uid,"revision":revision,"reply_revision":reply_revision,"thread_id":thread,"text":text,"language":language,"history_id":row_id}});
        if browser.sender.try_send(event).is_err() {
            return;
        }
        browser.pending.pop_front();
        browser.active = Some(uid.clone());
        mark_latency(
            inner,
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
pub(super) fn interrupt_client(inner: &mut Inner, sid: &str, reason: &str) {
    let mut rows = Vec::new();
    for record in inner.utterances.values_mut() {
        if let Some(entry) = record.clients.get_mut(sid) {
            if matches!(entry.1.as_str(), "queued" | "waiting_for_turn" | "playing") {
                entry.1 = "interrupted".into();
                if record.replay_of.is_none()
                    && record.clients.values().all(|(_, status)| {
                        !matches!(status.as_str(), "queued" | "waiting_for_turn" | "playing")
                    })
                {
                    rows.push(record.row_id.clone());
                }
            }
        }
    }
    for row_id in rows {
        if let Some(row) = inner.rows.iter_mut().find(|r| r.id == row_id) {
            if row.status != "playback_finished" {
                row.status = "interrupted".into();
                row.reason = Some(reason.into());
            }
        }
    }
    if let Some(browser) = inner.browsers.get_mut(sid) {
        browser.pending.clear();
        browser.active = None;
    }
    retire_terminal_replays(inner);
}
pub(super) fn hold_client(inner: &mut Inner, sid: &str, revision: u64) {
    let active = inner.browsers.get_mut(sid).and_then(|b| b.active.take());
    let mut waiting = Vec::new();
    let mut interrupted = Vec::new();
    for (uid, record) in &mut inner.utterances {
        if let Some(entry) = record.clients.get_mut(sid) {
            match entry.1.as_str() {
                "queued" | "waiting_for_turn" => {
                    entry.0 = revision;
                    entry.1 = "waiting_for_turn".into();
                    waiting.push((
                        uid.clone(),
                        record.replay_of.is_none().then(|| record.row_id.clone()),
                    ));
                }
                "playing" => {
                    entry.1 = "interrupted".into();
                    if record.replay_of.is_none() {
                        interrupted.push(record.row_id.clone());
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(uid) = active {
        if waiting.iter().any(|(id, _)| id == &uid) {
            if let Some(browser) = inner.browsers.get_mut(sid) {
                browser.pending.push_front(uid);
            }
        }
    }
    for (_, row_id) in waiting {
        if let Some(row_id) = row_id {
            sync_row(inner, &row_id, "waiting_for_turn", Some("user_speaking"));
        }
    }
    for row_id in interrupted {
        sync_row(inner, &row_id, "interrupted", Some("newer_turn"));
    }
    retire_terminal_replays(inner);
}
