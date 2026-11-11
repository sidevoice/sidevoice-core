//! The person's input still waiting for its agent, kept across a restart of the core.
//!
//! The journal is in memory, and so is everything about a call; what the person said and the room has taken must not
//! be lost with them. The rows of the person's input (and the notes that wait in the same order) that have not been
//! seen to reach their agent are written to `pending-input.json` in the same step that changes them (a message is taken,
//! delivered, read, cancelled), before the page is told it was taken, and read back, in the order they had, when the room
//! loads, each in its delivery phase (`push.rs`):
//! - `pending` and `sending` come back `pending`: whether the agent took one being sent is not known, so it is offered
//!   again with the same `message_id` (the connector never hands the agent an id twice);
//! - `delivered` (the agent took it, its receipt is awaited) comes back `delivered`: it is only asked about again, never
//!   offered;
//! - `delivered` and fetched by a pull comes back claimed: the next fetch of its conversation returns it until it is
//!   acknowledged, and it is never pushed.
//!
//! Nothing else of the journal is kept: a row already read is the agent's, and a restart ends the calls, so there is no
//! session to tell about them.
//!
//! A record that cannot be written does not stop a message: it is delivered all the same (the disk failing must not
//! silence the person), and the delivery pump writes what has changed and failed to be written before, every 250 ms, until
//! it can. Only a restart in that time would lose it. The journal's sequence number is kept too, so a pull cursor held from
//! before the restart still precedes what is restored.
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

    /// The person's input not yet seen to reach its agent, oldest first, each with its phase.
    fn pending_state(&self) -> Value {
        let rows: Vec<Value> = self
            .journal
            .kept_input()
            .map(|row| {
                let phase = if row.status == "delivered" {
                    "delivered"
                } else {
                    "pending"
                };
                json!({"id":row.id,"phase":phase,"pull_claimed_by":row.pull_claimed_by,"thread":row.thread,"role":row.role,"text":row.text,"name":row.name,"session":row.session,
                "revision":row.revision,"epoch":row.epoch,"turn_id":row.turn_id,"client_key":row.client_key,"time":row.time,
                "offline":row.offline,"payload":row.payload,"queued_at":row.queued_at})
            })
            .collect();
        json!({"seq": self.journal.last_seq(), "input": rows})
    }
}

impl Room {
    /// Writes the input still waiting if it changed since the last write (the pump calls it for what failed before).
    pub fn persist_pending(&self) {
        let _ = self.inner.lock().expect("room lock").persist_pending();
    }

    /// Whether everything the room has taken is kept for a restart. The call asks before it tells a page a message was
    /// taken: a page not told keeps the message and says it again, which never silences the person and never loses the
    /// message. Blocking (a file write): call it from a blocking task.
    pub fn ensure_durable(&self) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .persist_pending()
            .is_ok()
    }

    /// Puts back the input a previous run of the core was still holding, each in its phase (see the module).
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
        inner.journal.resume_seq(state["seq"].as_u64().unwrap_or(0));
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
            // A message the agent took stays taken: only whether it has it is asked again. One fetched by a pull stays
            // claimed, for whichever binding of its conversation fetches next (`transfer_claims`). Anything else, `sending`
            // included, is offered again.
            let delivered = saved["phase"].as_str() == Some("delivered");
            let pull_claimed_by = saved["pull_claimed_by"]
                .as_str()
                .filter(|_| delivered)
                .map(str::to_owned);
            inner
                .conversations
                .restore_epoch(thread, saved["epoch"].as_u64().unwrap_or(0));
            inner.append_row(Row {
                id: id.into(),
                thread: thread.into(),
                // The notes go in the same order as the words, so they are kept with them.
                role: if saved["role"].as_str() == Some("note") {
                    "note"
                } else {
                    "user"
                },
                text: text.into(),
                name: saved["name"].as_str().map(str::to_owned),
                session: saved["session"].as_str().unwrap_or("").into(),
                revision: saved["revision"].as_u64().unwrap_or(0),
                epoch: saved["epoch"].as_u64().unwrap_or(0),
                turn_id: saved["turn_id"].as_str().map(str::to_owned),
                // A record written before the client's name was kept: derived from the turn, or the message's own id.
                client_key: saved["client_key"].as_str().map(str::to_owned).or_else(|| {
                    match (saved["turn_id"].as_str(), payload["message_id"].as_str()) {
                        (Some(turn), _) => Some(format!("turn:{turn}")),
                        (None, Some(message)) => Some(format!("text:{message}")),
                        _ => None,
                    }
                }),
                time: saved["time"].as_u64().unwrap_or(0),
                status: if delivered { "delivered" } else { "pending" }.into(),
                pull_claimed_by,
                ever_exposed: delivered,
                // Phase 2 asks again soon after a restart; phase 1 offers at once.
                next_attempt: if delivered { seconds() + 1 } else { 0 },
                offline: saved.get("offline").filter(|v| !v.is_null()).cloned(),
                payload: Some(payload.clone()),
                queued_at: saved["queued_at"].as_u64().unwrap_or_else(seconds),
                ..Row::default()
            });
        }
        Ok(())
    }
}
