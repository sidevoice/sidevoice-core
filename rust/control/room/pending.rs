//! The person's input still waiting for its agent, kept across a restart of the core.
//!
//! The journal is in memory, and so is everything about a call; what the person said and the room has taken must not
//! be lost with them. The rows of the person's input that are `pending`, `sending`, or fetched by a pull and not yet
//! acknowledged are written to `pending-input.json` in the same step that changes them (a message is taken, delivered,
//! read, cancelled), before the page is told it was taken, and read back as `pending` rows, in the order they had, when
//! the room loads. A message taken by a pull and not acknowledged before the restart is offered again, with the same
//! `message_id`. Nothing else of the journal is kept: a row already delivered is the agent's, and a restart ends the
//! calls, so there is no session to tell about them.
//!
//! A message that cannot be written is not taken (`room.state_save_failed`): the page keeps it and says it again. The
//! delivery pump also writes every 250 ms what has changed and failed to be written before.
use std::io;

use serde_json::{json, Value};

use super::journal::Row;
use super::util::seconds;
use super::{Inner, Room};
use crate::storage::PrivateDir;

const FILE: &str = "pending-input.json";

/// The record of the input still waiting, and what was last written to it.
pub(super) struct PendingStore {
    dir: PrivateDir,
    written: String,
}
impl PendingStore {
    pub(super) fn new(dir: PrivateDir) -> Self {
        Self {
            dir,
            written: String::new(),
        }
    }
}

impl Inner {
    /// Writes the input still waiting if it changed since the last write.
    pub(super) fn persist_pending(&mut self) -> io::Result<()> {
        let state = self.pending_state();
        let text = state.to_string();
        if text == self.pending_store.written {
            return Ok(());
        }
        self.pending_store.dir.write_json(FILE, &state)?;
        self.pending_store.written = text;
        Ok(())
    }

    /// The person's input still waiting for its agent, oldest first.
    fn pending_state(&self) -> Value {
        let rows: Vec<Value> = self
            .journal
            .waiting_user_input()
            .map(|row| {
                json!({"id":row.id,"thread":row.thread,"text":row.text,"name":row.name,"session":row.session,
                "revision":row.revision,"epoch":row.epoch,"turn_id":row.turn_id,"time":row.time,
                "offline":row.offline,"payload":row.payload,"queued_at":row.queued_at})
            })
            .collect();
        json!({"input": rows})
    }
}

impl Room {
    /// Writes the input still waiting if it changed since the last write (the pump calls it for what failed before).
    pub fn persist_pending(&self) {
        let _ = self.inner.lock().expect("room lock").persist_pending();
    }

    /// Puts back the input a previous run of the core was still holding, as `pending`, to be delivered again.
    pub(super) fn restore_pending(&self) -> io::Result<()> {
        let state = match self.dir.read_json(FILE) {
            Ok(Some(state)) => state,
            Ok(None) => return Ok(()),
            // A record that does not parse is a record lost, not a core that cannot start.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut inner = self.inner.lock().expect("room lock");
        inner.pending_store.written = state.to_string();
        for saved in state["input"].as_array().into_iter().flatten() {
            let (Some(id), Some(thread), Some(text), Some(payload)) = (
                saved["id"].as_str(),
                saved["thread"].as_str(),
                saved["text"].as_str(),
                saved.get("payload").filter(|payload| payload.is_object()),
            ) else {
                continue;
            };
            if !inner.journal.can_append() || inner.journal.find(id).is_some() {
                continue;
            }
            inner.append_row(Row {
                id: id.into(),
                thread: thread.into(),
                role: "user",
                text: text.into(),
                name: saved["name"].as_str().map(str::to_owned),
                session: saved["session"].as_str().unwrap_or("").into(),
                revision: saved["revision"].as_u64().unwrap_or(0),
                epoch: saved["epoch"].as_u64().unwrap_or(0),
                turn_id: saved["turn_id"].as_str().map(str::to_owned),
                time: saved["time"].as_u64().unwrap_or(0),
                status: "pending".into(),
                offline: saved.get("offline").filter(|v| !v.is_null()).cloned(),
                payload: Some(payload.clone()),
                queued_at: saved["queued_at"].as_u64().unwrap_or_else(seconds),
                ..Row::default()
            });
        }
        Ok(())
    }
}
