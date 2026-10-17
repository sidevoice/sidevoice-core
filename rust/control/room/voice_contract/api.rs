//! V1 connector and browser entrypoints.
use super::*;

impl Room {
    #[expect(
        clippy::too_many_arguments,
        reason = "browser receipt fields mirror the wire frame"
    )]
    pub fn contract_playback(
        &self,
        sid: &str,
        uid: &str,
        aid: &str,
        status: &str,
        hearing: Option<&str>,
        heard_chars: Option<u64>,
        reason: Option<&str>,
    ) -> Result<(), RoomError> {
        let invalid = || RoomError::new(400, "room.receipt_invalid");
        let stale = || RoomError::new(409, "room.stale_utterance");
        let mut inner = self.inner.lock().expect("room lock");
        let Some(attempt) = inner
            .voice_contract
            .attempts
            .get(aid)
            .filter(|a| {
                a.original == uid && a.listeners.iter().any(|(listener, _)| listener == sid)
            })
            .cloned()
        else {
            return Err(stale());
        };
        let Some(original) = inner.voice_contract.originals.get(uid) else {
            return Err(stale());
        };
        let length = original.text.chars().count() as u64;
        if heard_chars.is_some_and(|n| n > length) {
            return Err(invalid());
        }
        let extent = match hearing {
            Some("complete") if heard_chars == Some(length) => Some(length),
            Some("partial") if heard_chars.is_some_and(|n| n > 0 && n < length) => heard_chars,
            Some("not_played") if heard_chars == Some(0) => Some(0),
            Some("unknown") | None if heard_chars.is_none() => None,
            _ => return Err(invalid()),
        };
        // Refuse every invalid receipt before changing the hearing fact, pending delta, or
        // attempt. A rejected terminal status must not certify a completed original.
        let terminal = match status {
            "playing" => None,
            "heard" => Some("completed"),
            "interrupted" => Some("partial"),
            "unplayed" => Some("not_started"),
            "failed" => Some("failed"),
            _ => return Err(invalid()),
        };
        if let Some(extent) = extent {
            let fact = inner
                .voice_contract
                .hearing
                .entry((uid.to_owned(), sid.to_owned()))
                .or_default();
            let advanced = fact.confirmed.is_none_or(|previous| extent > previous)
                || (hearing == Some("complete") && !fact.complete);
            if advanced {
                fact.confirmed = Some(fact.confirmed.unwrap_or(0).max(extent));
                fact.complete |= hearing == Some("complete") || fact.confirmed == Some(length);
                let state = if fact.complete {
                    "complete"
                } else if extent == 0 {
                    "not_played"
                } else {
                    "partial"
                };
                let confirmed = fact.confirmed;
                inner
                    .voice_contract
                    .return_noted
                    .remove(&(sid.to_owned(), uid.to_owned()));
                let pending = inner
                    .voice_contract
                    .changes
                    .entry(attempt.thread.clone())
                    .or_default();
                let fact = json!({"listener_session_id":sid,
                    "utterance_id":uid,"attempt_id":aid,"hearing":state,
                    "heard_chars":confirmed});
                let mut truncated = false;
                if let Some(previous) = pending
                    .iter_mut()
                    .find(|change| change.original == uid && change.listener == sid)
                {
                    previous.fact = fact;
                } else {
                    if pending.len() >= MAX_PENDING_FACTS {
                        pending.remove(0);
                        truncated = true;
                    }
                    pending.push(PendingHearing {
                        original: uid.into(),
                        listener: sid.into(),
                        fact,
                    });
                }
                if truncated {
                    inner
                        .voice_contract
                        .changes_truncated
                        .insert(attempt.thread.clone());
                }
            }
        }
        if terminal.is_none() {
            let mut active = false;
            if let Some(stored) = inner.voice_contract.attempts.get_mut(aid) {
                if matches!(stored.state, "queued" | "fenced") {
                    stored.state = "playing";
                }
                active = stored.state == "playing";
            }
            if active {
                inner.contract_mark_active(uid, "playing");
            }
            return Ok(());
        }
        let terminal = terminal.expect("validated terminal status");
        if let Some(stored) = inner.voice_contract.attempts.get_mut(aid) {
            stored.terminal.insert(sid.to_owned(), terminal.into());
            if stored.terminal.len() == stored.listeners.len() && stored.state != "terminal" {
                let outcome = if stored.terminal.values().all(|value| value == "completed") {
                    "completed"
                } else if stored
                    .terminal
                    .values()
                    .any(|value| matches!(value.as_str(), "partial" | "completed"))
                {
                    "partial"
                } else if stored.terminal.values().any(|value| value == "failed") {
                    "failed"
                } else {
                    "not_started"
                };
                inner.contract_end_attempt(aid, outcome, reason);
                if outcome != "completed"
                    && !matches!(reason, Some("newer_turn" | "new_words_pending_only"))
                {
                    if let Some(ended) = inner.voice_contract.attempts.get(aid).cloned() {
                        inner.contract_handoff_recovery(&ended);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn contract_publish(&self, cid: &str, p: &Value, recovery: bool) -> Value {
        let eid = field(p, "event_id");
        let uid = field(p, "utterance_id");
        let text = field(p, "text");
        if !valid_id(eid) || !valid_id(uid) || text.is_empty() || text.chars().count() > 6000 {
            return rejected("invalid_request", eid, uid);
        }
        let bid = field(p, "binding_id");
        let fingerprint = fingerprint(json!([
            bid,
            p.get("voice_contract_instance"),
            eid,
            uid,
            text,
            p.get("language"),
            p.get("session_id"),
            p.get("revision"),
            p.get("epoch"),
            p.get("context_message_id")
        ]));
        let mut inner = self.inner.lock().expect("room lock");
        let now = seconds();
        inner.contract_expire(now);
        let Some(binding) = inner
            .bindings
            .get(bid)
            .filter(|binding| binding.connector == cid && !binding.room_closed)
        else {
            return rejected("binding_inactive", eid, uid);
        };
        let owned_live = binding.active && binding.live;
        let recovery_live = recovery
            && p.get("recovery_binding_id")
                .and_then(Value::as_str)
                .and_then(|new_bid| inner.bindings.live_of(cid, new_bid))
                .is_some_and(|new_binding| {
                    new_binding.active
                        && new_binding.thread == binding.thread
                        && !binding.client_ref.is_empty()
                        && new_binding.client_ref == binding.client_ref
                });
        if !owned_live && !recovery_live {
            return rejected("binding_inactive", eid, uid);
        }
        if !inner.voice_contract.instance_matches(bid, p) {
            return rejected("binding_inactive", eid, uid);
        }
        let thread = binding.thread.clone();
        if let Some((old, result)) = inner.voice_contract.event_keys.get(eid) {
            return if *old == fingerprint {
                result.clone()
            } else {
                rejected("identity_conflict", eid, uid)
            };
        }
        if let Some((old, result)) = inner.voice_contract.original_keys.get(uid) {
            return if *old == fingerprint {
                result.clone()
            } else {
                rejected("identity_conflict", eid, uid)
            };
        }
        if inner.voice_contract.event_keys.len() >= MAX_IDEMPOTENCY_KEYS {
            return rejected("capacity_full", eid, uid);
        }
        let effective_epoch = match inner.contract_context(&thread, bid, p, true) {
            Ok(epoch) => epoch,
            Err(reason) => {
                let result = rejected(reason, eid, uid);
                inner
                    .voice_contract
                    .remember_event(eid, fingerprint, result.clone());
                return result;
            }
        };
        if (!recovery
            && (inner.voice_contract.active_count(&thread) >= MAX_PENDING
                || !inner.voice_contract.can_insert_attempt()))
            || !inner.journal.can_append()
            || inner.room_for_bytes(text.len() + 2048).is_err()
        {
            let result = rejected("capacity_full", eid, uid);
            inner
                .voice_contract
                .remember_event(eid, fingerprint, result.clone());
            return result;
        }
        if !inner.voice_contract.reserve_original(text.len()) {
            let result = rejected("capacity_full", eid, uid);
            inner
                .voice_contract
                .remember_event(eid, fingerprint, result.clone());
            return result;
        }
        // An uncertain first admission may be saved, but must never start late sound.
        inner.contract_save(p, &thread, fingerprint, recovery, now, effective_epoch)
    }

    pub fn contract_get_reply(&self, cid: &str, p: &Value) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let bid = field(p, "binding_id");
        let uid = field(p, "utterance_id");
        let Some(binding) = inner
            .bindings
            .get(bid)
            .filter(|b| b.connector == cid && !b.room_closed)
        else {
            return json!({"status":"unavailable"});
        };
        let owned_live = binding.active && binding.live;
        let replacement_live = p
            .get("recovery_binding_id")
            .and_then(Value::as_str)
            .and_then(|new_bid| inner.bindings.live_of(cid, new_bid))
            .is_some_and(|new_binding| {
                new_binding.active
                    && new_binding.thread == binding.thread
                    && !binding.client_ref.is_empty()
                    && new_binding.client_ref == binding.client_ref
            });
        if !owned_live && !replacement_live {
            return json!({"status":"unavailable"});
        }
        if !inner.voice_contract.instance_matches(bid, p) {
            return json!({"status":"unavailable"});
        }
        let Some(original) = inner.voice_contract.originals.get(uid).filter(|o| {
            o.binding_id == bid
                && o.thread == binding.thread
                && seconds().saturating_sub(o.created) < ORIGINAL_TTL
        }) else {
            return json!({"status":"unavailable"});
        };
        let targets = inner.eligible_listeners(&binding.thread);
        let mut minimum = original.text.chars().count() as u64;
        let mut uncertain = targets.is_empty();
        let mut complete = !targets.is_empty();
        for (sid, _) in &targets {
            let fact = inner
                .voice_contract
                .hearing
                .get(&(uid.to_owned(), sid.clone()));
            minimum = minimum.min(fact.and_then(|fact| fact.confirmed).unwrap_or(0));
            uncertain |= fact.and_then(|fact| fact.confirmed).is_none();
            complete &= fact.is_some_and(|fact| fact.complete);
        }
        if targets.is_empty() {
            minimum = 0;
        }
        json!({"status":"available","utterance_id":uid,"text":original.text,
            "audience_hearing":{"confirmed_for_all_chars":minimum,"complete_for_all":complete,
            "uncertain":uncertain,"listener_count":targets.len()}})
    }

    pub fn contract_replay(&self, cid: &str, p: &Value) -> Value {
        let rid = field(p, "replay_id");
        let uid = field(p, "utterance_id");
        let bid = field(p, "binding_id");
        let mode = p.get("mode").and_then(Value::as_str).unwrap_or("resume");
        if !valid_id(rid) || !valid_id(uid) || !matches!(mode, "resume" | "restart") {
            return json!({"status":"rejected","reason":"invalid_request"});
        }
        let fingerprint = fingerprint(json!([
            bid,
            p.get("voice_contract_instance"),
            rid,
            uid,
            mode,
            p.get("session_id"),
            p.get("revision"),
            p.get("epoch"),
            p.get("context_message_id")
        ]));
        let mut inner = self.inner.lock().expect("room lock");
        let now = seconds();
        inner.contract_expire(now);
        let Some(binding) = inner
            .bindings
            .get(bid)
            .filter(|binding| binding.connector == cid && !binding.room_closed)
        else {
            return json!({"status":"rejected","reason":"binding_inactive"});
        };
        let owned_live = binding.active && binding.live;
        let recovery_live = p
            .get("recovery_binding_id")
            .and_then(Value::as_str)
            .and_then(|new_bid| inner.bindings.live_of(cid, new_bid))
            .is_some_and(|new_binding| {
                new_binding.active
                    && new_binding.thread == binding.thread
                    && !binding.client_ref.is_empty()
                    && new_binding.client_ref == binding.client_ref
            });
        if !inner.voice_contract.instance_matches(bid, p) {
            return json!({"status":"rejected","reason":"binding_inactive"});
        }
        if let Some((old, result)) = inner.voice_contract.replay_keys.get(rid) {
            if !owned_live && !recovery_live {
                return json!({"status":"rejected","reason":"binding_inactive"});
            }
            return if *old == fingerprint {
                result.clone()
            } else {
                json!({"status":"rejected","reason":"identity_conflict"})
            };
        }
        if !owned_live && !recovery_live {
            return json!({"status":"rejected","reason":"binding_inactive"});
        }
        if inner.voice_contract.replay_keys.len() >= MAX_IDEMPOTENCY_KEYS {
            return json!({"status":"rejected","reason":"capacity_full"});
        }
        let active_bid = if owned_live {
            bid
        } else {
            field(p, "recovery_binding_id")
        };
        let result = inner.contract_replay_new(cid, p, mode, now, active_bid);
        inner
            .voice_contract
            .remember_replay(rid, fingerprint, result.clone());
        result
    }
}
