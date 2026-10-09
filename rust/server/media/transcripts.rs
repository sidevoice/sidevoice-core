//! Device transcription requests awaiting the browser's reply, keyed by request id.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use serde_json::Value;
use tokio::sync::oneshot;

/// `Err(())` is the browser reporting that its own transcription failed.
pub(super) type DeviceReply = Result<Option<String>, ()>;

type Pending = HashMap<String, oneshot::Sender<DeviceReply>>;

#[derive(Default)]
pub(super) struct DeviceTranscripts {
    pending: Mutex<Pending>,
}

impl DeviceTranscripts {
    pub(super) fn open(&self, request: &str) -> oneshot::Receiver<DeviceReply> {
        let (tx, rx) = oneshot::channel();
        self.lock().insert(request.to_owned(), tx);
        rx
    }

    pub(super) fn forget(&self, request: &str) {
        self.lock().remove(request);
    }

    /// Answers the matching request of this session; replies for others are ignored. True when a
    /// request was waiting for this reply.
    pub(super) fn resolve(&self, data: &Value, error: bool, session: &str) -> bool {
        if data.get("session_id").and_then(Value::as_str) != Some(session) {
            return false;
        }
        let Some(request) = data.get("request_id").and_then(Value::as_str) else {
            return false;
        };
        let pending = self.lock().remove(request);
        let Some(pending) = pending else {
            return false;
        };
        let _ = pending.send(if error { Err(()) } else { Ok(reply_text(data)) });
        true
    }

    pub(super) fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().expect("transcripts lock")
    }
}

pub(super) fn reply_text(data: &Value) -> Option<String> {
    data.get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}
