//! App-reported semantic hearing, independent of physical playback and its stop cause.
use super::error::RoomError;
use super::Room;

impl Room {
    /// A device confirms how much of a retained assistant message it delivered. Audio admission
    /// is not required: a text-only reply seen in reading mode has the same identity and receipt.
    pub fn reply_hearing(
        &self,
        sid: &str,
        history_id: &str,
        utterance_id: &str,
        outcome: &str,
        heard_chars: Option<u64>,
    ) -> Result<(), RoomError> {
        let invalid = || RoomError::new(400, "room.receipt_invalid");
        let stale = || RoomError::new(409, "room.stale_utterance");
        if !matches!(outcome, "heard" | "partial" | "not_heard") {
            return Err(invalid());
        }
        let mut inner = self.inner.lock().expect("room lock");
        let row = inner
            .journal
            .find(history_id)
            .filter(|row| row.role == "assistant")
            .ok_or_else(stale)?;
        let thread = row.thread.clone();
        let session = row.session.clone();
        let epoch = row.epoch;
        let length = row.text.chars().count() as u64;
        let original = history_id
            .rsplit_once(":voice:")
            .map(|(_, id)| id)
            .ok_or_else(stale)?;
        if utterance_id != original {
            let replay = inner.utterances.get(utterance_id).ok_or_else(stale)?;
            if !replay.is_replay()
                || replay.row_id != history_id
                || !replay.clients.contains_key(sid)
            {
                return Err(stale());
            }
        }
        if !inner.browsers.contains(sid) {
            return Err(stale());
        }
        let extent = match (outcome, heard_chars) {
            ("heard", None) => length,
            ("not_heard", None) => 0,
            ("partial", Some(n)) if n > 0 && n < length => n,
            _ => return Err(invalid()),
        };
        let row = inner.journal.find_mut(history_id).ok_or_else(stale)?;
        let contract_reply = row
            .publication
            .as_ref()
            .is_some_and(|result| result["voice_contract"] == 1);
        let previous = row.hearing_reports.get(sid).cloned();
        let stronger = previous
            .as_ref()
            .is_none_or(|(_, prior_extent)| extent > *prior_extent);
        if !stronger {
            return Ok(());
        }
        row.hearing_reports
            .insert(sid.to_owned(), (outcome.to_owned(), extent));
        let maximum = row
            .hearing_reports
            .values()
            .map(|(_, extent)| *extent)
            .max()
            .unwrap_or(0);
        let all_reported = row
            .hearing_expected
            .iter()
            .all(|target| row.hearing_reports.contains_key(target));
        row.globally_heard = row.globally_heard || outcome == "heard" || maximum == length;
        row.hearing_unconfirmed = !row.globally_heard && !all_reported;
        row.hearing = Some(
            if row.globally_heard {
                "heard"
            } else if maximum > 0
                || row
                    .hearing_reports
                    .values()
                    .any(|(outcome, _)| outcome == "partial")
            {
                "partial"
            } else if !row.hearing_unconfirmed {
                "not_heard"
            } else {
                row.heard_chars = None;
                row.hearing = None;
                inner.reconcile_unclaimed(&thread);
                return Ok(());
            }
            .to_owned(),
        );
        row.heard_chars = Some(if row.globally_heard { length } else { maximum });
        if contract_reply {
            // Physical contract receipts own recovery and replay decisions. This older semantic
            // projection remains available in history without generating a second recovery note.
            return Ok(());
        }
        if row.globally_heard {
            inner.unheard.update(&thread, history_id, false, None);
            let listeners: Vec<String> = inner
                .utterances
                .get(original)
                .into_iter()
                .flat_map(|record| record.clients.iter())
                .filter(|(listener, (_, status))| listener.as_str() != sid && status == "queued")
                .map(|(listener, _)| listener.clone())
                .collect();
            for listener in listeners {
                inner.withdraw(&listener, "heard_elsewhere", |record, (_, status)| {
                    record.row_id == history_id && status == "queued"
                });
            }
        } else {
            inner.track_unheard(history_id);
        }
        inner.reconcile_unclaimed(&thread);
        if !inner.conversations.is_open(&thread) {
            let message = inner.journal.newer_spoken_message(&thread, epoch);
            let empty = inner.conversations.empty_after(&thread, epoch);
            match (message, empty) {
                (Some((word_epoch, message_id)), Some((empty_epoch, _)))
                    if word_epoch <= empty_epoch =>
                {
                    inner.flush_all_unheard_notes(
                        &thread,
                        &session,
                        "reply_not_heard",
                        Some(&message_id),
                    );
                }
                (_, Some((_, empty_session))) => {
                    inner.flush_all_unheard_notes(&thread, &empty_session, "reply_not_heard", None);
                }
                (Some((_, message_id)), None) => {
                    inner.flush_all_unheard_notes(
                        &thread,
                        &session,
                        "reply_not_heard",
                        Some(&message_id),
                    );
                }
                (None, None) if inner.conversations.epoch(&thread) > epoch => {
                    inner.flush_all_unheard_notes(&thread, &session, "reply_not_heard", None);
                }
                (None, None) => {}
            }
        }
        Ok(())
    }
}
