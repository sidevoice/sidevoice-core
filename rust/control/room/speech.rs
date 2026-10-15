//! Admission of an agent reply under the conversation's current speech epoch.
use serde_json::{json, Value};

use super::journal::Row;
use super::util::{default_title, field, millis, seconds, valid_thread};
use super::utterances::{UtteranceRecord, MAX_UTTERANCES};
use super::{Inner, Room};

const SPEECH_LANGUAGES: [&str; 6] = ["es", "en", "fr", "it", "pt", "hi"];

impl Room {
    pub fn publish(&self, p: &Value) -> Value {
        self.publish_with_policy(p, false)
    }

    /// Resolve an uncertain publication without allowing its first admission to speak late.
    pub fn recover_speech(&self, cid: &str, p: &Value) -> Value {
        let bid = field(p, "recovery_binding_id");
        let original_bid = field(p, "binding_id");
        let thread = {
            let inner = self.inner.lock().expect("room lock");
            let Some(binding) = inner.bindings.live_of(cid, bid) else {
                return json!({"status":"unknown_binding","event_id":p.get("event_id"),
                    "utterance_id":p.get("utterance_id")});
            };
            if inner.bindings.get(original_bid).is_some_and(|original| {
                original.thread != binding.thread || original.connector != cid
            }) {
                return rejected("room.speech_context_invalid");
            }
            binding.thread.clone()
        };
        let mut speech = p.clone();
        speech["thread_id"] = json!(thread);
        self.publish_with_policy(&speech, true)
    }

    fn publish_with_policy(&self, p: &Value, no_late_first_admission: bool) -> Value {
        let thread = field(p, "thread_id");
        let uid = field(p, "utterance_id");
        let text = field(p, "text");
        let sid = field(p, "session_id");
        let (Some(revision), Some(epoch)) = (
            p.get("revision").and_then(Value::as_u64),
            p.get("epoch").and_then(Value::as_u64),
        ) else {
            return rejected("room.speech_invalid");
        };
        if text.is_empty()
            || text.chars().count() > 6000
            || uid.is_empty()
            || uid.len() > 200
            || !valid_thread(thread)
            || sid.is_empty()
        {
            return rejected("room.speech_invalid");
        }
        let bid = field(p, "binding_id");
        let fingerprint = json!([
            bid,
            thread,
            sid,
            revision,
            epoch,
            text,
            p.get("event_id"),
            p.get("language"),
            p.get("history_only")
        ]);
        let mut guard = self.inner.lock().expect("room lock");
        let inner = &mut *guard;
        if let Some((prior, disposition)) = inner.refusals.get(uid) {
            return if *prior == fingerprint {
                disposition.clone()
            } else {
                rejected("room.speech_conflict")
            };
        }
        if let Some(row) = inner
            .utterances
            .get(uid)
            .and_then(|record| inner.journal.find(&record.row_id))
            .or_else(|| inner.journal.find(&format!("{sid}:voice:{uid}")))
        {
            return if row.publication_context.as_ref() == Some(&fingerprint) {
                row.publication
                    .clone()
                    .unwrap_or_else(|| rejected("room.speech_conflict"))
            } else {
                rejected("room.speech_conflict")
            };
        }
        let initial = sid == format!("binding:{bid}")
            && revision == 0
            && inner
                .bindings
                .get(bid)
                .is_some_and(|binding| binding.thread == thread && binding.active);
        if !no_late_first_admission && !inner.browsers.is_recent(sid) && !initial {
            let disposition = rejected("room.speech_context_invalid");
            inner.remember_refusal(uid, fingerprint, disposition.clone());
            return disposition;
        }
        if !inner.conversations.ensure(thread) {
            let disposition = rejected("room.input_backlog_full");
            inner.remember_refusal(uid, fingerprint, disposition.clone());
            return disposition;
        }
        // An open turn is a terminal refusal. Remember it through retries after that turn closes.
        if !no_late_first_admission && inner.conversations.is_open(thread) {
            let disposition = json!({"status":"rejected","terminal":true,"reason":"user_speaking",
                "error":"room.user_speaking","instruction":"The person is speaking; this will not play. Wait for their message."});
            inner.remember_refusal(uid, fingerprint, disposition.clone());
            inner.empty_obligation(thread);
            return disposition;
        }
        let current = inner.conversations.epoch(thread);
        if !no_late_first_admission && epoch > current {
            let disposition = rejected("room.speech_context_invalid");
            inner.remember_refusal(uid, fingerprint, disposition.clone());
            return disposition;
        }
        if inner
            .room_for_bytes(text.len() + 2048 + inner.unheard.pending_bytes(&inner.journal))
            .is_err()
        {
            let disposition = rejected("room.input_backlog_full");
            inner.remember_refusal(uid, fingerprint, disposition.clone());
            return disposition;
        }
        let mut audience = inner.browsers.ids_on_thread(thread);
        audience.sort();
        audience.retain(|listener| {
            inner
                .browsers
                .get(listener)
                .is_some_and(|browser| browser.parked.is_none() && browser.speech_on)
        });
        let capacity = inner.utterances.original_count() >= MAX_UTTERANCES;
        let reason = if no_late_first_admission {
            Some("connector_disconnected".to_owned())
        } else if epoch < current {
            Some("newer_turn".to_owned())
        } else if p.get("history_only").and_then(Value::as_bool) == Some(true) {
            Some("connector_disconnected".to_owned())
        } else if capacity {
            Some("queue_full".to_owned())
        } else if audience.is_empty() {
            let on_thread: Vec<_> = inner
                .browsers
                .on_thread(thread)
                .map(|(_, browser)| browser)
                .collect();
            Some(
                if on_thread
                    .iter()
                    .any(|browser| browser.speech_reason.as_deref() == Some("agents_muted"))
                {
                    "agents_muted"
                } else if on_thread
                    .iter()
                    .any(|browser| browser.speech_reason.as_deref() == Some("voice_off"))
                {
                    "voice_off"
                } else if on_thread.iter().any(|browser| browser.parked.is_some()) {
                    "disconnected"
                } else if inner.browsers.contains(sid) {
                    "inactive_conversation"
                } else if inner.browsers.is_recent(sid) {
                    "call_ended"
                } else {
                    "session_changed"
                }
                .to_owned(),
            )
        } else {
            None
        };
        let status = if reason.is_some() {
            "text_only"
        } else {
            "queued"
        };
        let language = p
            .get("language")
            .and_then(Value::as_str)
            .filter(|language| SPEECH_LANGUAGES.contains(language));
        let asker = inner.browsers.get(sid);
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
        let row_id = format!("{sid}:voice:{uid}");
        let disposition = if let Some(ref reason) = reason {
            json!({"status":status,"utterance_id":uid,"session_id":sid,"revision":revision,
                "epoch":epoch,"text_saved":true,"reason":reason})
        } else {
            json!({"status":status,"utterance_id":uid,"session_id":sid,"revision":revision,
                "epoch":epoch,"text_saved":true})
        };
        inner.append_row(Row {
            id: row_id.clone(),
            thread: thread.into(),
            role: "assistant",
            text: text.into(),
            name: Some(name),
            session: sid.into(),
            revision,
            epoch,
            time: millis(),
            status: status.into(),
            reason: reason.clone(),
            language: language.map(str::to_owned),
            queued_at: seconds(),
            publication: Some(disposition.clone()),
            publication_context: Some(fingerprint.clone()),
            ..Row::default()
        });
        if reason.is_none() {
            inner.speak(uid, row_id, &audience, thread, revision, epoch);
        } else {
            inner.track_unheard(&row_id);
            if !capacity {
                inner.utterances.insert(
                    uid,
                    UtteranceRecord {
                        row_id,
                        epoch,
                        parked: true,
                        ..Default::default()
                    },
                );
            }
            if reason.as_deref() == Some("newer_turn") {
                inner.flush_all_unheard_notes(thread, sid, "reply_not_played", None);
            }
        }
        inner.remember_refusal(uid, fingerprint, disposition.clone());
        disposition
    }

    pub fn connector_speech(&self, cid: &str, p: &Value) -> Value {
        let bid = field(p, "binding_id");
        let thread = {
            let inner = self.inner.lock().expect("room lock");
            inner.bindings.live_of(cid, bid).map(|b| b.thread.clone())
        };
        let Some(thread) = thread else {
            return json!({"status":"unknown_binding","event_id":p.get("event_id"),
                "utterance_id":p.get("utterance_id")});
        };
        let mut speech = p.clone();
        speech["thread_id"] = json!(thread);
        self.publish(&speech)
    }
}

impl Inner {
    fn remember_refusal(&mut self, uid: &str, fingerprint: Value, disposition: Value) {
        if self
            .refusals
            .insert(uid.to_owned(), (fingerprint, disposition))
            .is_none()
        {
            self.refusal_order.push_back(uid.to_owned());
        }
        while self.refusal_order.len() > 2048 {
            if let Some(old) = self.refusal_order.pop_front() {
                self.refusals.remove(&old);
            }
        }
    }

    fn speak(
        &mut self,
        uid: &str,
        row_id: String,
        audience: &[String],
        thread: &str,
        revision: u64,
        epoch: u64,
    ) {
        let clients = audience
            .iter()
            .filter_map(|listener| {
                let c = self.browsers.get(listener)?;
                Some((listener.clone(), (c.revision, "queued".to_owned())))
            })
            .collect();
        self.utterances.insert(
            uid,
            UtteranceRecord {
                row_id,
                epoch,
                clients,
                ..Default::default()
            },
        );
        for listener in audience {
            self.register_latency_reply(listener, thread, revision, uid, "queued");
            self.send_reply(listener, uid);
        }
    }
}

fn rejected(key: &str) -> Value {
    json!({"status":"rejected","error":key,"terminal":true,"reason_code":"application_refusal"})
}
