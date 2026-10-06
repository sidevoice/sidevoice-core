//! Speech publication: an agent's reply becoming a journal row and an utterance for its audience.
use std::collections::HashMap;

use serde_json::{json, Value};

use super::journal::{trim_rows, Row};
use super::latency::register_latency_reply;
use super::playback::{dispatch_client, MAX_PENDING};
use super::util::{field, id, millis, seconds, valid_thread};
use super::Room;

pub(super) const MAX_UTTERANCES: usize = 2048;

pub(super) struct UtteranceRecord {
    pub(super) row_id: String,
    pub(super) clients: HashMap<String, (u64, String)>,
    pub(super) parked: bool,
    pub(super) replay_of: Option<String>,
}

impl Room {
    pub fn publish(&self, p: &Value, v3: bool) -> Value {
        let original_sid = field(p, "session_id");
        let thread = field(p, "thread_id");
        let requested_uid = field(p, "utterance_id");
        let text = field(p, "text");
        let generated = id();
        let uid = if requested_uid.is_empty() && !v3 {
            generated.as_str()
        } else {
            requested_uid
        };
        let mut revision = p.get("revision").and_then(Value::as_u64).unwrap_or(0);
        if text.is_empty()
            || text.len() > 6000
            || uid.len() > 200
            || !valid_thread(thread)
            || p.get("revision").and_then(Value::as_u64).is_none()
            || p.get("language")
                .and_then(Value::as_str)
                .is_some_and(|language| !["es", "en", "fr", "it", "pt", "hi"].contains(&language))
        {
            return json!({"status":"rejected","error":"room.speech_invalid","terminal":v3,"reason_code":"application_refusal"});
        }
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(record) = inner.utterances.get(uid) {
            if let Some(row) = inner.rows.iter().find(|r| r.id == record.row_id) {
                return if row.text == text && row.thread == thread {
                    json!({"status":row.status,"text_saved":true,"utterance_id":uid})
                } else {
                    json!({"status":"rejected","error":"room.speech_invalid","terminal":v3,"reason_code":"application_refusal"})
                };
            }
        }
        let audience: Vec<String> = inner
            .browsers
            .iter()
            .filter(|(_, b)| b.target.as_ref().is_some_and(|t| t.thread == thread))
            .map(|(id, _)| id.clone())
            .collect();
        let mut sid = original_sid.to_owned();
        if !inner.browsers.contains_key(original_sid) {
            if let Some(replacement) = inner
                .sessions
                .iter()
                .rev()
                .find(|candidate| audience.contains(candidate))
            {
                sid = replacement.clone();
                revision = inner.browsers.get(&sid).map_or(revision, |b| b.revision);
            }
        }
        let row_id = format!("{sid}:voice:{uid}");
        if let Some(row) = inner.rows.iter().find(|r| r.id == row_id) {
            return if row.text == text && row.thread == thread {
                json!({"status":row.status,"text_saved":true,"utterance_id":uid})
            } else {
                json!({"status":"rejected","error":"room.speech_invalid","terminal":v3,"reason_code":"application_refusal"})
            };
        }
        let asker = inner.browsers.get(&sid);
        let known = inner.sessions.iter().any(|s| s == &sid);
        let reason = if !known {
            Some("session_changed")
        } else if asker.is_none() {
            Some("call_ended")
        } else if asker.is_some_and(|b| b.target.as_ref().is_none_or(|t| t.thread != thread)) {
            Some("focus_changed")
        } else if asker.is_some_and(|b| b.revision != revision) {
            Some(if asker.is_some_and(|b| b.turn_revision > revision) {
                "newer_turn"
            } else {
                "focus_changed"
            })
        } else if asker.is_some_and(|b| b.speaking) {
            Some("user_speaking")
        } else {
            None
        };
        let capacity = inner
            .utterances
            .values()
            .filter(|record| record.replay_of.is_none())
            .count()
            >= MAX_UTTERANCES
            || audience.iter().any(|id| {
                inner
                    .browsers
                    .get(id)
                    .is_some_and(|b| b.pending.len() >= MAX_PENDING)
            });
        let can_speak = reason.is_none_or(|r| ["newer_turn", "user_speaking"].contains(&r))
            && !audience.is_empty()
            && !capacity;
        let waiting = can_speak
            && audience
                .iter()
                .any(|id| inner.browsers.get(id).is_some_and(|b| b.speaking));
        let status = if !can_speak {
            "text_only"
        } else if waiting {
            "waiting_for_turn"
        } else {
            "queued"
        };
        let spoken_revision =
            if reason.is_some_and(|r| ["newer_turn", "user_speaking"].contains(&r)) {
                asker.map_or(revision, |b| b.revision)
            } else {
                revision
            };
        let reason = if capacity { Some("queue_full") } else { reason };
        let name = inner
            .bindings
            .values()
            .filter(|b| b.thread == thread && b.active)
            .max_by_key(|b| b.created)
            .and_then(|b| b.title.clone())
            .or_else(|| {
                asker
                    .and_then(|b| b.target.as_ref())
                    .and_then(|t| t.title.clone())
            })
            .or_else(|| {
                Some(crate::messages::render(
                    &crate::messages::LocalizedMessage::new("room.conversation_title")
                        .with_param("id", thread.chars().take(8).collect::<String>()),
                    asker.map_or("en", |b| b.language.as_str()),
                ))
            });
        inner.seq += 1;
        let seq = inner.seq;
        inner.rows.push_back(Row {
            seq,
            id: row_id.clone(),
            thread: thread.into(),
            role: "assistant",
            text: text.into(),
            name,
            session: sid.clone(),
            revision,
            time: millis(),
            status: status.into(),
            reason: reason.map(str::to_owned),
            language: p.get("language").and_then(Value::as_str).map(str::to_owned),
            offline: None,
            payload: None,
            queued_at: seconds(),
            attempts: 0,
            next_attempt: 0,
            pull_claimed_by: None,
        });
        trim_rows(&mut inner);
        if can_speak {
            let mut clients = HashMap::new();
            for listener in &audience {
                if let Some(c) = inner.browsers.get(listener) {
                    let client_status = if c.speaking {
                        "waiting_for_turn"
                    } else {
                        "queued"
                    };
                    clients.insert(listener.clone(), (c.revision, client_status.into()));
                }
            }
            inner.utterances.insert(
                uid.into(),
                UtteranceRecord {
                    row_id: row_id.clone(),
                    clients,
                    parked: false,
                    replay_of: None,
                },
            );
            for listener in &audience {
                register_latency_reply(&mut inner, listener, thread, revision, uid, status);
            }
            for listener in audience {
                if let Some(c) = inner.browsers.get_mut(&listener) {
                    c.pending.push_back(uid.into());
                }
                dispatch_client(&mut inner, &listener);
            }
        }
        if status == "text_only"
            && matches!(
                reason,
                Some("session_changed" | "call_ended" | "focus_changed")
            )
            && inner
                .utterances
                .values()
                .filter(|record| record.replay_of.is_none())
                .count()
                < MAX_UTTERANCES
        {
            inner.utterances.insert(
                uid.into(),
                UtteranceRecord {
                    row_id,
                    clients: HashMap::new(),
                    parked: true,
                    replay_of: None,
                },
            );
        }
        if status == "text_only" {
            json!({"status":"text_only","text_saved":true,"reason":reason.unwrap_or("call_ended")})
        } else {
            json!({"status":status,"utterance_id":uid,"session_id":sid,"revision":spoken_revision,"text_saved":true})
        }
    }
    pub fn connector_speech(&self, cid: &str, p: &Value, v3: bool) -> Value {
        let bid = field(p, "binding_id");
        let thread = {
            let inner = self.inner.lock().expect("room lock");
            inner
                .bindings
                .get(bid)
                .filter(|b| b.connector == cid && b.live)
                .map(|b| b.thread.clone())
        };
        let Some(thread) = thread else {
            return if v3 {
                json!({"status":"unknown_binding","event_id":p.get("event_id"),"utterance_id":p.get("utterance_id")})
            } else {
                json!({"status":"rejected","error":"room.binding_foreign","event_id":p.get("event_id")})
            };
        };
        let mut speech = p.clone();
        speech["thread_id"] = json!(thread);
        self.publish(&speech, v3)
    }
}
