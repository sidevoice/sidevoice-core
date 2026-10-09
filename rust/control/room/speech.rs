//! Speech publication: an agent's reply becoming a journal row and an utterance for its audience.

use serde_json::{json, Value};

use super::browsers::Browser;
use super::journal::Row;
use super::playback::MAX_PENDING;
use super::util::{default_title, field, id, millis, seconds, valid_thread};
use super::utterances::{UtteranceRecord, MAX_UTTERANCES};
use super::{Inner, Room};

const SPEECH_LANGUAGES: [&str; 6] = ["es", "en", "fr", "it", "pt", "hi"];

impl Room {
    pub fn publish(&self, p: &Value, v3: bool) -> Value {
        let thread = field(p, "thread_id");
        let requested_uid = field(p, "utterance_id");
        let text = field(p, "text");
        let generated = id();
        let uid = if requested_uid.is_empty() && !v3 {
            generated.as_str()
        } else {
            requested_uid
        };
        let Some(revision) = p.get("revision").and_then(Value::as_u64) else {
            return rejected(v3);
        };
        if text.is_empty()
            || text.chars().count() > 6000
            || uid.len() > 200
            || !valid_thread(thread)
            || p.get("language")
                .and_then(Value::as_str)
                .is_some_and(|language| !SPEECH_LANGUAGES.contains(&language))
        {
            return rejected(v3);
        }
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if let Some(row) = inner
            .utterances
            .get(uid)
            .and_then(|record| inner.journal.find(&record.row_id))
        {
            return repeated(row, text, thread, uid, v3);
        }
        let audience = inner.browsers.ids_on_thread(thread);
        let (sid, revision) = inner.speaker(field(p, "session_id"), revision, &audience);
        let row_id = format!("{sid}:voice:{uid}");
        if let Some(row) = inner.journal.find(&row_id) {
            return repeated(row, text, thread, uid, v3);
        }
        let asker = inner.browsers.get(&sid);
        let reason = refusal(inner.browsers.is_recent(&sid), asker, thread, revision);
        // A reply to a turn the person already followed with a message is never spoken.
        let deferred = reason.is_some_and(deferrable)
            && !(reason == Some("newer_turn")
                && inner.journal.has_newer_input(&sid, thread, revision));
        let capacity = inner.utterances.original_count() >= MAX_UTTERANCES
            || audience.iter().any(|id| {
                inner
                    .browsers
                    .get(id)
                    .is_some_and(|b| b.pending.len() >= MAX_PENDING)
            });
        let can_speak = (reason.is_none() || deferred) && !audience.is_empty() && !capacity;
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
        let spoken_revision = if deferred {
            asker.map_or(revision, |b| b.revision)
        } else {
            revision
        };
        let reason = if capacity { Some("queue_full") } else { reason };
        let name = inner
            .bindings
            .newest_active(thread)
            .and_then(|b| b.title.clone())
            .or_else(|| {
                asker
                    .and_then(|b| b.target.as_ref())
                    .and_then(|t| t.title.clone())
            })
            .unwrap_or_else(|| default_title(thread, asker.map_or("en", |b| b.language.as_str())));
        inner.append_row(Row {
            id: row_id.clone(),
            thread: thread.into(),
            role: "assistant",
            text: text.into(),
            name: Some(name),
            session: sid.clone(),
            revision,
            time: millis(),
            status: status.into(),
            reason: reason.map(str::to_owned),
            language: p.get("language").and_then(Value::as_str).map(str::to_owned),
            queued_at: seconds(),
            ..Row::default()
        });
        if !can_speak {
            inner.track_unheard(&row_id);
        }
        if can_speak {
            inner.speak(uid, row_id, &audience, thread, revision, status);
        } else if matches!(
            reason,
            Some("session_changed" | "call_ended" | "focus_changed")
        ) && inner.utterances.original_count() < MAX_UTTERANCES
        {
            // Nobody heard it: keep it so that it can still be replayed later.
            inner.utterances.insert(
                uid,
                UtteranceRecord {
                    row_id,
                    parked: true,
                    ..Default::default()
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
            inner.bindings.live_of(cid, bid).map(|b| b.thread.clone())
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

impl Inner {
    /// The call a reply is spoken for, and the revision it answers: the call that asked if it
    /// is still here, otherwise the most recent call listening to the thread at its revision.
    fn speaker(&self, asked: &str, revision: u64, audience: &[String]) -> (String, u64) {
        if !self.browsers.contains(asked) {
            if let Some(replacement) = self.browsers.most_recent_of(audience) {
                let revision = self
                    .browsers
                    .get(replacement)
                    .map_or(revision, |b| b.revision);
                return (replacement.clone(), revision);
            }
        }
        (asked.to_owned(), revision)
    }
    /// Queue a reply for every call in its audience and start playing where nothing else is.
    fn speak(
        &mut self,
        uid: &str,
        row_id: String,
        audience: &[String],
        thread: &str,
        revision: u64,
        status: &str,
    ) {
        let clients = audience
            .iter()
            .filter_map(|listener| {
                let c = self.browsers.get(listener)?;
                let client_status = if c.speaking {
                    "waiting_for_turn"
                } else {
                    "queued"
                };
                Some((listener.clone(), (c.revision, client_status.to_owned())))
            })
            .collect();
        self.utterances.insert(
            uid,
            UtteranceRecord {
                row_id,
                clients,
                ..Default::default()
            },
        );
        for listener in audience {
            self.register_latency_reply(listener, thread, revision, uid, status);
        }
        for listener in audience {
            if let Some(c) = self.browsers.get_mut(listener) {
                c.pending.push_back(uid.into());
            }
            self.dispatch_client(listener);
        }
    }
}

/// Why a reply cannot be spoken to the call it answers as it stands, if it cannot.
fn refusal(
    known: bool,
    asker: Option<&Browser>,
    thread: &str,
    revision: u64,
) -> Option<&'static str> {
    let Some(asker) = asker else {
        return Some(if known {
            "call_ended"
        } else {
            "session_changed"
        });
    };
    if !known {
        Some("session_changed")
    } else if !asker.is_on(thread) {
        Some("focus_changed")
    } else if asker.revision != revision {
        Some(if asker.turn_revision > revision {
            "newer_turn"
        } else {
            "focus_changed"
        })
    } else if asker.speaking {
        Some("user_speaking")
    } else {
        None
    }
}

/// Reasons a reply can still be spoken for, after the user's current turn.
fn deferrable(reason: &str) -> bool {
    ["newer_turn", "user_speaking"].contains(&reason)
}

fn rejected(v3: bool) -> Value {
    json!({"status":"rejected","error":"room.speech_invalid","terminal":v3,"reason_code":"application_refusal"})
}

/// The answer to a reply whose row already exists: accepted again if it is the same words on
/// the same thread, refused otherwise.
fn repeated(row: &Row, text: &str, thread: &str, uid: &str, v3: bool) -> Value {
    if row.text == text && row.thread == thread {
        json!({"status":row.status,"text_saved":true,"utterance_id":uid})
    } else {
        rejected(v3)
    }
}
