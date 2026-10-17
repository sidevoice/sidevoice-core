//! Recovery ownership and relevant return notes.
use super::*;

impl Inner {
    pub(super) fn contract_handoff_recovery(&mut self, attempt: &Attempt) {
        if !attempt.auto_resume
            || self
                .voice_contract
                .recovery_handed_off
                .contains(&attempt.original)
        {
            return;
        }
        let listener = attempt.listeners.iter().find_map(|(sid, _)| {
            self.browsers
                .get(sid)
                .filter(|browser| {
                    browser.parked.is_none()
                        && browser.speech_on
                        && browser
                            .bound_target()
                            .is_some_and(|target| target.thread == attempt.thread)
                })
                .filter(|_| {
                    attempt
                        .terminal
                        .get(sid)
                        .is_none_or(|status| status != "completed")
                })
                .map(|browser| (sid.clone(), browser.revision))
        });
        let Some((sid, revision)) = listener else {
            return;
        };
        let facts: Vec<Value> = attempt
            .listeners
            .iter()
            .map(|(listener, _)| {
                let hearing = self
                    .voice_contract
                    .hearing
                    .get(&(attempt.original.clone(), listener.clone()));
                let (state, chars) = match hearing.and_then(|h| h.confirmed) {
                    Some(0) => ("not_played", Some(0)),
                    Some(n) if hearing.is_some_and(|h| h.complete) => ("complete", Some(n)),
                    Some(n) => ("partial", Some(n)),
                    None => ("unknown", None),
                };
                json!({"listener_session_id":listener,"utterance_id":attempt.original,
                "attempt_id":attempt.id,"hearing":state,"heard_chars":chars})
            })
            .collect();
        if self
            .room_for_bytes(2048 + serde_json::to_string(&facts).expect("hearing facts").len())
            .is_err()
        {
            return;
        }
        let outcomes = self
            .voice_contract
            .outcomes
            .remove(&attempt.thread)
            .unwrap_or_default();
        let message_id = format!("note:{}", id());
        let mut payload = json!({"channel":"note","event":"recovery_needed","message_id":message_id,
            "session_id":sid,"revision":revision,"epoch":self.conversations.epoch(&attempt.thread),
            "thread_id":attempt.thread,"hearing_changes":facts,"playback_outcomes":outcomes});
        if self
            .voice_contract
            .outcomes_truncated
            .remove(&attempt.thread)
        {
            payload["playback_outcomes_truncated"] = json!(true);
        }
        if let Some(changes) = self.voice_contract.changes.get_mut(&attempt.thread) {
            changes.retain(|change| {
                change.original != attempt.original
                    || !attempt
                        .listeners
                        .iter()
                        .any(|(listener, _)| listener == &change.listener)
            });
        }
        self.append_row(Row {
            id: message_id,
            thread: attempt.thread.clone(),
            role: "note",
            text: String::new(),
            session: sid,
            revision,
            epoch: self.conversations.epoch(&attempt.thread),
            time: millis(),
            status: "pending".into(),
            queued_at: seconds(),
            payload: Some(payload),
            ..Row::default()
        });
        self.voice_contract
            .recovery_handed_off
            .insert(attempt.original.clone());
    }

    pub(in crate::control::room) fn contract_offer_return_note(&mut self, sid: &str) {
        let Some(browser) = self.browsers.get(sid) else {
            return;
        };
        if browser.parked.is_some() || !browser.speech_on {
            return;
        }
        let Some(thread) = browser.bound_target().map(|target| target.thread.clone()) else {
            return;
        };
        let now = seconds();
        let mut facts = Vec::new();
        for uid in &self.voice_contract.original_order {
            let Some(original) = self
                .voice_contract
                .originals
                .get(uid)
                .filter(|o| o.thread == thread && now.saturating_sub(o.created) < ORIGINAL_TTL)
            else {
                continue;
            };
            if self
                .voice_contract
                .return_noted
                .contains(&(sid.to_owned(), uid.clone()))
            {
                continue;
            }
            let heard = self
                .voice_contract
                .hearing
                .get(&(uid.clone(), sid.to_owned()));
            if heard.is_some_and(|h| h.complete) {
                continue;
            }
            let (hearing, chars) = match heard.and_then(|h| h.confirmed) {
                Some(0) => ("not_played", Some(0)),
                Some(n) => ("partial", Some(n)),
                None if original.result["playback"] == "text_only" => ("not_played", Some(0)),
                None => ("unknown", None),
            };
            facts.push(
                json!({"utterance_id":uid,"attempt_id":original.result["attempt_id"],
                "hearing":hearing,"heard_chars":chars}),
            );
        }
        if facts.is_empty()
            || self
                .room_for_bytes(2048 + json!(facts).to_string().len())
                .is_err()
        {
            return;
        }
        let message_id = format!("note:{}", id());
        let revision = browser.revision;
        let epoch = self.conversations.epoch(&thread);
        let mut payload = json!({"channel":"note","event":"user_returned","message_id":message_id,
            "session_id":sid,"revision":revision,"epoch":epoch,"hearing_changes":facts,
            "playback_outcomes":self.voice_contract.outcomes.remove(&thread).unwrap_or_default(),
            "thread_id":thread});
        if self.voice_contract.outcomes_truncated.remove(&thread) {
            payload["playback_outcomes_truncated"] = json!(true);
        }
        if let Some(changes) = self.voice_contract.changes.get_mut(&thread) {
            changes.retain(|change| {
                change.listener != sid
                    || !facts
                        .iter()
                        .any(|fact| field(fact, "utterance_id") == change.original)
            });
        }
        for fact in facts {
            self.voice_contract
                .return_noted
                .insert((sid.to_owned(), field(&fact, "utterance_id").into()));
        }
        self.append_row(Row {
            id: message_id,
            thread,
            role: "note",
            text: String::new(),
            session: sid.into(),
            revision,
            epoch,
            time: millis(),
            status: "pending".into(),
            queued_at: now,
            payload: Some(payload),
            ..Row::default()
        });
    }
}
