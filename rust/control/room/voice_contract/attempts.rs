//! Admission context, physical attempts, dispatch, and turn fences.
use super::*;

impl Inner {
    pub(super) fn contract_expire(&mut self, now: u64) {
        let expired = self.voice_contract.cleanup(now);
        for attempt in expired {
            for (sid, _) in &attempt.listeners {
                if let Some(browser) = self.browsers.get(sid) {
                    browser.critical(json!({"type":"voice-reply-withdrawn","data":{
                        "session_id":sid,"utterance_ids":[attempt.original],
                        "attempt_ids":[attempt.id],"reason":"expired_pending_only"}}));
                }
            }
            self.contract_update_row(&attempt.id, "not_started", Some("expired"));
            self.contract_handoff_recovery(&attempt);
        }
        self.voice_contract.expire_originals(now);
    }

    pub(in crate::control::room) fn contract_update_row(
        &mut self,
        aid: &str,
        outcome: &str,
        reason: Option<&str>,
    ) {
        let Some(attempt) = self.voice_contract.attempts.get(aid) else {
            return;
        };
        let Some(original) = self.voice_contract.originals.get(&attempt.original) else {
            return;
        };
        let row_id = original.row_id.clone();
        let active = self
            .voice_contract
            .attempt_order
            .iter()
            .rev()
            .filter_map(|id| self.voice_contract.attempts.get(id))
            .find(|other| other.original == attempt.original && other.state != "terminal");
        let status = active
            .map(|other| {
                if other.state == "playing" {
                    "playing"
                } else {
                    "queued"
                }
            })
            .unwrap_or(match outcome {
                "completed" => "heard",
                "partial" => "interrupted",
                "failed" => "failed",
                _ => "unplayed",
            });
        if let Some(row) = self.journal.find_mut(&row_id) {
            row.status = status.into();
            row.reason = if active.is_some() {
                None
            } else {
                reason.map(str::to_owned)
            };
        }
    }

    pub(in crate::control::room) fn contract_end_attempt(
        &mut self,
        aid: &str,
        outcome: &str,
        reason: Option<&str>,
    ) {
        if self
            .voice_contract
            .attempts
            .get(aid)
            .is_none_or(|a| a.state == "terminal")
        {
            return;
        }
        self.voice_contract.end_attempt(aid, outcome, reason);
        self.contract_update_row(aid, outcome, reason);
    }

    pub(in crate::control::room) fn contract_mark_active(&mut self, uid: &str, state: &str) {
        let Some(original) = self.voice_contract.originals.get(uid) else {
            return;
        };
        if let Some(row) = self.journal.find_mut(&original.row_id) {
            row.status = if state == "playing" {
                "playing"
            } else {
                "queued"
            }
            .into();
            row.reason = None;
        }
    }

    pub(in crate::control::room) fn eligible_listeners(
        &self,
        thread: &str,
    ) -> Vec<(String, String)> {
        let mut listeners: Vec<_> = self
            .browsers
            .on_thread(thread)
            .filter(|(_, browser)| browser.parked.is_none() && browser.speech_on)
            .filter_map(|(sid, browser)| {
                Some((sid.clone(), browser.target.as_ref()?.binding_id.clone()))
            })
            .collect();
        listeners.sort();
        listeners
    }

    pub(in crate::control::room) fn contract_context(
        &self,
        thread: &str,
        bid: &str,
        p: &Value,
        allow_initial: bool,
    ) -> Result<u64, &'static str> {
        let sid = p.get("session_id").and_then(Value::as_str);
        let revision = p.get("revision").and_then(Value::as_u64);
        let epoch = p.get("epoch").and_then(Value::as_u64);
        let context = p.get("context_message_id").and_then(Value::as_str);
        let absent = ["session_id", "revision", "epoch", "context_message_id"]
            .iter()
            .all(|key| p.get(*key).is_none());
        let latest = self.journal.latest_context(thread);
        if absent && allow_initial && latest.is_none() && !self.journal.has_seen_input(thread) {
            return Ok(self.conversations.epoch(thread));
        }
        if absent {
            return Err("context_stale");
        }
        let (Some(sid), Some(revision), Some(epoch)) = (sid, revision, epoch) else {
            return Err("invalid_request");
        };
        if let Some(row) = latest {
            if context != Some(row.message_id())
                || sid != row.session
                || revision != row.revision
                || epoch != row.epoch
            {
                return Err("context_stale");
            }
        } else if allow_initial
            && !self.journal.has_seen_input(thread)
            && p.get("context_message_id").is_none()
            && self.voice_contract.initial_sessions.get(bid).is_some_and(
                |(initial, registered_epoch)| initial == sid && *registered_epoch == epoch,
            )
            && revision == 0
        {
            // An initial context from this incarnation is valid before any room input.
        } else {
            return Err("context_stale");
        }
        if !self.empty_or_open_lineage(thread, epoch) {
            return Err("context_stale");
        }
        Ok(epoch)
    }

    pub(in crate::control::room) fn empty_or_open_lineage(&self, thread: &str, from: u64) -> bool {
        let current = self.conversations.epoch(thread);
        if from > current {
            return false;
        }
        ((from + 1)..=current).all(|epoch| {
            self.conversations
                .empty_after(thread, epoch.saturating_sub(1))
                .is_some_and(|(ended, _)| ended == epoch)
                || self.browsers.iter().any(|(_, browser)| {
                    browser.turns.iter().any(|turn| {
                        turn.thread_id.as_deref() == Some(thread) && turn.epoch == epoch
                    })
                })
        })
    }

    pub(in crate::control::room) fn contract_save(
        &mut self,
        p: &Value,
        thread: &str,
        fingerprint: Value,
        force_text_only: bool,
        now: u64,
        effective_epoch: u64,
    ) -> Value {
        let uid = field(p, "utterance_id");
        let eid = field(p, "event_id");
        let bid = field(p, "binding_id");
        let text = field(p, "text");
        let listeners = self.eligible_listeners(thread);
        let pending = self.conversations.is_open(thread);
        let playback = if force_text_only || listeners.is_empty() {
            "text_only"
        } else if pending {
            "pending"
        } else {
            "queued"
        };
        let attempt_id = (playback != "text_only").then(id);
        let result = json!({"status":"accepted","event_id":eid,"utterance_id":uid,
            "playback":playback,"attempt_id":attempt_id,"voice_contract":1});
        let sid = p
            .get("session_id")
            .and_then(Value::as_str)
            .or_else(|| {
                self.voice_contract
                    .initial_sessions
                    .get(bid)
                    .map(|(sid, _)| sid.as_str())
            })
            .unwrap_or("")
            .to_owned();
        let revision = p.get("revision").and_then(Value::as_u64).unwrap_or(0);
        let epoch = effective_epoch;
        let row_id = format!("{sid}:voice:{uid}");
        self.append_row(Row {
            id: row_id.clone(),
            thread: thread.into(),
            role: "assistant",
            text: text.into(),
            session: sid,
            revision,
            epoch,
            time: millis(),
            status: playback.into(),
            hearing_unconfirmed: true,
            hearing_expected: listeners.iter().map(|(sid, _)| sid.clone()).collect(),
            language: p.get("language").and_then(Value::as_str).map(str::to_owned),
            queued_at: now,
            publication: Some(result.clone()),
            publication_context: Some(fingerprint.clone()),
            ..Row::default()
        });
        self.voice_contract.originals.insert(
            uid.into(),
            Original {
                utterance_id: uid.into(),
                binding_id: bid.into(),
                thread: thread.into(),
                row_id,
                text: text.into(),
                created: now,
                result: result.clone(),
            },
        );
        self.voice_contract.original_order.push_back(uid.into());
        self.voice_contract
            .remember_original(uid, fingerprint.clone(), result.clone());
        self.voice_contract
            .remember_event(eid, fingerprint, result.clone());
        if let Some(aid) = attempt_id {
            let attempt = Attempt {
                id: aid.clone(),
                original: uid.into(),
                thread: thread.into(),
                binding_id: bid.into(),
                epoch,
                listeners: listeners.clone(),
                start_chars: 0,
                deadline: now + AUTO_TTL,
                requested: false,
                auto_resume: false,
                typed_fenced: false,
                state: if pending { "pending" } else { "queued" },
                terminal: HashMap::new(),
            };
            let _ = self.voice_contract.insert_attempt(attempt);
            if pending {
                self.voice_contract.pending.push_back(aid.clone());
                for (listener, _) in listeners {
                    if let Some(browser) = self.browsers.get(&listener) {
                        browser.notify(json!({"type":"voice-reply-admitted","data":{
                            "session_id":listener,"thread_id":thread,"utterance_id":uid,"attempt_id":aid}}));
                    }
                }
            } else {
                self.dispatch_contract(&aid);
            }
        }
        result
    }

    pub(in crate::control::room) fn contract_replay_new(
        &mut self,
        cid: &str,
        p: &Value,
        mode: &str,
        now: u64,
        active_bid: &str,
    ) -> Value {
        let uid = field(p, "utterance_id");
        let rid = field(p, "replay_id");
        let bid = field(p, "binding_id");
        let Some(binding) = self.bindings.live_of(cid, active_bid).filter(|b| b.active) else {
            return json!({"status":"rejected","reason":"binding_inactive"});
        };
        let thread = binding.thread.clone();
        let Some(original) = self.voice_contract.originals.get(uid).filter(|o| {
            o.binding_id == bid
                && o.thread == thread
                && now.saturating_sub(o.created) < ORIGINAL_TTL
        }) else {
            return json!({"status":"failed","reason":"unavailable"});
        };
        let length = original.text.chars().count() as u64;
        if let Err(reason) = self.contract_context(&thread, active_bid, p, false) {
            return json!({"status":"rejected","reason":reason});
        }
        let listeners = self.eligible_listeners(&thread);
        if listeners.is_empty() {
            return json!({"status":"rejected","reason":"no_listener"});
        }
        let all_complete = listeners.iter().all(|(sid, _)| {
            self.voice_contract
                .hearing
                .get(&(uid.into(), sid.clone()))
                .is_some_and(|h| h.complete)
        });
        if mode == "resume" && all_complete {
            return json!({"status":"already_heard","utterance_id":uid});
        }
        if self.voice_contract.active_count(&thread) >= MAX_PENDING {
            return json!({"status":"rejected","reason":"capacity_full"});
        }
        let minimum = listeners
            .iter()
            .map(|(sid, _)| {
                self.voice_contract
                    .hearing
                    .get(&(uid.into(), sid.clone()))
                    .and_then(|h| h.confirmed)
                    .unwrap_or(0)
            })
            .min()
            .unwrap_or(0);
        let start_chars = if mode == "restart" {
            0
        } else {
            sentence_start(&original.text, minimum.min(length))
        };
        let pending = self.conversations.is_open(&thread);
        let aid = id();
        if !self.voice_contract.insert_attempt(Attempt {
            id: aid.clone(),
            original: uid.into(),
            thread: thread.clone(),
            binding_id: active_bid.into(),
            epoch: self.conversations.epoch(&thread),
            listeners,
            start_chars,
            deadline: now + AUTO_TTL,
            requested: true,
            auto_resume: false,
            typed_fenced: false,
            state: if pending { "pending" } else { "queued" },
            terminal: HashMap::new(),
        }) {
            return json!({"status":"rejected","reason":"capacity_full"});
        }
        // A new authorized replay starts a distinct recovery ownership episode for this
        // original. Late receipts for the prior terminal attempt cannot hand off again.
        self.voice_contract.recovery_handed_off.remove(uid);
        self.contract_mark_active(uid, if pending { "pending" } else { "queued" });
        if pending {
            self.voice_contract.pending.push_back(aid.clone());
        } else {
            self.dispatch_contract(&aid);
        }
        json!({"status":"accepted","utterance_id":uid,"replay_id":rid,"attempt_id":aid,
            "playback":if pending {"pending"} else {"queued"}})
    }

    pub(in crate::control::room) fn dispatch_contract(&mut self, aid: &str) {
        let Some(attempt) = self.voice_contract.attempts.get(aid).cloned() else {
            return;
        };
        if attempt.state == "terminal" || self.conversations.is_open(&attempt.thread) {
            return;
        }
        if attempt.deadline <= seconds() {
            self.contract_end_attempt(aid, "not_started", Some("expired"));
            self.contract_handoff_recovery(&attempt);
            return;
        }
        if !self.empty_or_open_lineage(&attempt.thread, attempt.epoch) {
            self.contract_end_attempt(aid, "not_started", Some("context_stale"));
            self.contract_handoff_recovery(&attempt);
            return;
        }
        let Some(original) = self
            .voice_contract
            .originals
            .get(&attempt.original)
            .cloned()
        else {
            self.contract_end_attempt(aid, "failed", Some("unavailable"));
            self.contract_handoff_recovery(&attempt);
            return;
        };
        if seconds().saturating_sub(original.created) >= ORIGINAL_TTL {
            self.contract_end_attempt(aid, "failed", Some("unavailable"));
            self.contract_handoff_recovery(&attempt);
            self.voice_contract.expire_originals(seconds());
            return;
        }
        if !self
            .bindings
            .get(&attempt.binding_id)
            .is_some_and(|b| b.active && b.live && b.thread == attempt.thread)
        {
            self.contract_end_attempt(aid, "not_started", Some("binding_inactive"));
            self.contract_handoff_recovery(&attempt);
            return;
        }
        let start_chars = if attempt.start_chars == 0
            && attempt.epoch < self.conversations.epoch(&attempt.thread)
        {
            let minimum = attempt
                .listeners
                .iter()
                .map(|(sid, _)| {
                    self.voice_contract
                        .hearing
                        .get(&(attempt.original.clone(), sid.clone()))
                        .and_then(|h| h.confirmed)
                        .unwrap_or(0)
                })
                .min()
                .unwrap_or(0);
            sentence_start(&original.text, minimum)
        } else {
            attempt.start_chars
        };
        let mut offered = false;
        let mut still_needed = false;
        for (sid, focus) in &attempt.listeners {
            if attempt.terminal.contains_key(sid) {
                continue;
            }
            if attempt.auto_resume
                && self
                    .voice_contract
                    .hearing
                    .get(&(attempt.original.clone(), sid.clone()))
                    .is_some_and(|heard| heard.complete)
            {
                continue;
            }
            still_needed = true;
            let Some(browser) = self.browsers.get(sid).filter(|browser| {
                browser.parked.is_none()
                    && browser.speech_on
                    && browser.target.as_ref().is_some_and(|target| {
                        target.thread == attempt.thread && target.binding_id == *focus
                    })
            }) else {
                continue;
            };
            let listener_start = if attempt.auto_resume {
                let confirmed = self
                    .voice_contract
                    .hearing
                    .get(&(attempt.original.clone(), sid.clone()))
                    .and_then(|heard| heard.confirmed)
                    .unwrap_or(0);
                sentence_start(&original.text, confirmed)
            } else {
                start_chars
            };
            let frame = json!({"type":"voice-reply","data":{"session_id":sid,
                "utterance_id":original.utterance_id,"attempt_id":aid,"revision":browser.revision,
                "reply_revision":self.journal.find(&original.row_id).map_or(0, |r| r.revision),
                "epoch":self.conversations.epoch(&attempt.thread),"thread_id":attempt.thread,"text":original.text,
                "language":self.journal.find(&original.row_id).and_then(|r| r.language.clone()),
                "history_id":original.row_id,"replay":attempt.requested,
                    "requested":attempt.requested,"replay_start_chars":listener_start,
                    "auto_start_deadline_ms":attempt.deadline.saturating_mul(1000)}});
            offered |= browser.offer(frame);
        }
        if !offered {
            if !still_needed && attempt.auto_resume {
                self.contract_end_attempt(aid, "completed", Some("already_heard"));
            } else {
                self.contract_end_attempt(aid, "not_started", Some("no_listener"));
                self.contract_handoff_recovery(&attempt);
            }
        } else if let Some(stored) = self.voice_contract.attempts.get_mut(aid) {
            stored.state = "queued";
            stored.start_chars = start_chars;
        }
        if offered {
            self.contract_mark_active(&attempt.original, "queued");
        }
        self.voice_contract
            .pending
            .retain(|candidate| candidate != aid);
    }

    pub(in crate::control::room) fn dispatch_pending_contract(&mut self, thread: &str) {
        if self.conversations.is_open(thread) {
            return;
        }
        let due: Vec<_> = self
            .voice_contract
            .pending
            .iter()
            .filter(|aid| {
                self.voice_contract
                    .attempts
                    .get(*aid)
                    .is_some_and(|a| a.thread == thread)
            })
            .cloned()
            .collect();
        for aid in due {
            self.dispatch_contract(&aid);
        }
    }

    pub(in crate::control::room) fn revoke_pending_contract(&mut self, thread: &str, reason: &str) {
        let pending: Vec<_> = self
            .voice_contract
            .pending
            .iter()
            .filter(|aid| {
                self.voice_contract
                    .attempts
                    .get(*aid)
                    .is_some_and(|a| a.thread == thread)
            })
            .cloned()
            .collect();
        for aid in pending {
            self.contract_end_attempt(&aid, "not_started", Some(reason));
        }
    }

    pub(in crate::control::room) fn revoke_queued_contract(&mut self, thread: &str, _reason: &str) {
        let queued: Vec<Attempt> = self
            .voice_contract
            .attempts
            .values()
            .filter(|a| a.thread == thread && a.state == "queued")
            .cloned()
            .collect();
        for attempt in queued {
            for (sid, _) in &attempt.listeners {
                if let Some(browser) = self.browsers.get(sid) {
                    browser.critical(json!({"type":"voice-reply-withdrawn","data":{
                        "session_id":sid,"utterance_ids":[attempt.original],
                        "attempt_ids":[attempt.id],"reason":"new_words_pending_only"}}));
                }
            }
            if let Some(stored) = self.voice_contract.attempts.get_mut(&attempt.id) {
                stored.state = "fenced";
                stored.typed_fenced = true;
            }
        }
    }

    pub(in crate::control::room) fn contract_binding_lost(&mut self, bid: &str) {
        let affected: Vec<Attempt> = self
            .voice_contract
            .attempt_order
            .iter()
            .filter_map(|aid| self.voice_contract.attempts.get(aid))
            .filter(|attempt| attempt.binding_id == bid && attempt.state != "terminal")
            .cloned()
            .collect();
        for attempt in affected {
            if attempt.state == "pending" {
                self.contract_end_attempt(&attempt.id, "not_started", Some("binding_inactive"));
                self.contract_handoff_recovery(&attempt);
                continue;
            }
            for (sid, _) in &attempt.listeners {
                if let Some(browser) = self.browsers.get(sid) {
                    browser.critical(json!({"type":"voice-reply-withdrawn","data":{
                        "session_id":sid,"utterance_ids":[attempt.original],
                        "attempt_ids":[attempt.id],"reason":"binding_inactive_pending_only"}}));
                }
            }
            // A delayed playing receipt may prove that sound had already started. Keep that
            // physical attempt open; the browser withdrawal only fences local unstarted audio.
            if let Some(stored) = self.voice_contract.attempts.get_mut(&attempt.id) {
                if stored.state == "queued" {
                    stored.state = "fenced";
                }
            }
        }
    }

    pub(in crate::control::room) fn contract_turn_open(&mut self, thread: &str, epoch: u64) {
        let now = seconds();
        self.contract_expire(now);
        let active: Vec<Attempt> = self
            .voice_contract
            .attempt_order
            .iter()
            .filter_map(|aid| self.voice_contract.attempts.get(aid))
            .filter(|attempt| {
                attempt.thread == thread && matches!(attempt.state, "queued" | "playing")
            })
            .cloned()
            .collect();
        for attempt in active {
            for (sid, _) in &attempt.listeners {
                if let Some(browser) = self.browsers.get(sid) {
                    browser.critical(json!({"type":"voice-reply-withdrawn","data":{
                        "session_id":sid,"utterance_ids":[attempt.original],
                        "attempt_ids":[attempt.id],"reason":"newer_turn"}}));
                }
            }
            self.contract_end_attempt(
                &attempt.id,
                if attempt.state == "playing" {
                    "partial"
                } else {
                    "cancelled"
                },
                Some("user_interrupted"),
            );
            let original_fresh = self
                .voice_contract
                .originals
                .get(&attempt.original)
                .is_some_and(|original| now.saturating_sub(original.created) < ORIGINAL_TTL);
            let binding_live = self
                .bindings
                .get(&attempt.binding_id)
                .is_some_and(|binding| binding.active && binding.live);
            if original_fresh
                && binding_live
                && !attempt.typed_fenced
                && self.voice_contract.active_count(thread) < MAX_PENDING
            {
                let aid = id();
                let mut resumed = attempt.clone();
                resumed.id = aid.clone();
                resumed.epoch = epoch.saturating_sub(1);
                resumed.deadline = seconds() + AUTO_TTL;
                resumed.state = "pending";
                resumed.auto_resume = true;
                resumed.terminal.clear();
                resumed.listeners.retain(|(sid, _)| {
                    !self
                        .voice_contract
                        .hearing
                        .get(&(attempt.original.clone(), sid.clone()))
                        .is_some_and(|heard| heard.complete)
                });
                if !resumed.listeners.is_empty() && self.voice_contract.insert_attempt(resumed) {
                    self.contract_mark_active(&attempt.original, "pending");
                    self.voice_contract.pending.push_back(aid);
                }
            }
        }
        self.voice_contract.expire_originals(now);
    }

    pub(in crate::control::room) fn contract_focus_lost(&mut self, sid: &str) {
        let active: Vec<Attempt> = self
            .voice_contract
            .attempts
            .values()
            .filter(|attempt| {
                matches!(attempt.state, "queued" | "playing" | "pending" | "fenced")
                    && attempt
                        .listeners
                        .iter()
                        .any(|(listener, _)| listener == sid)
            })
            .cloned()
            .collect();
        for attempt in active {
            if attempt.state != "pending" {
                if let Some(browser) = self.browsers.get(sid) {
                    browser.critical(json!({"type":"voice-reply-withdrawn","data":{
                        "session_id":sid,"utterance_ids":[attempt.original],
                        "attempt_ids":[attempt.id],"reason":"focus_changed"}}));
                }
            }
            // Losing the transport is not evidence that an offered reply never sounded.
            // Keep a confirmed prefix; without one the physical result remains unknown.
            let listener_outcome = if attempt.state == "pending" {
                "not_started"
            } else {
                match self
                    .voice_contract
                    .hearing
                    .get(&(attempt.original.clone(), sid.to_owned()))
                {
                    Some(fact) if fact.complete => "completed",
                    Some(fact) if fact.confirmed.is_some_and(|chars| chars > 0) => "partial",
                    _ => "unknown",
                }
            };
            if let Some(stored) = self.voice_contract.attempts.get_mut(&attempt.id) {
                stored.terminal.insert(sid.into(), listener_outcome.into());
                if stored.terminal.len() == stored.listeners.len() {
                    let outcome = if stored.terminal.values().all(|value| value == "completed") {
                        "completed"
                    } else if stored
                        .terminal
                        .values()
                        .any(|value| matches!(value.as_str(), "partial" | "completed"))
                    {
                        "partial"
                    } else if stored
                        .terminal
                        .values()
                        .any(|value| matches!(value.as_str(), "unknown" | "failed"))
                    {
                        "failed"
                    } else {
                        "not_started"
                    };
                    self.contract_end_attempt(&attempt.id, outcome, Some("focus_changed"));
                }
            }
        }
    }
}
