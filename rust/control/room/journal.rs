//! The in-memory conversation journal: its rows, their bound, history and input receipts.
use serde_json::{json, Value};

use super::{Inner, Room};

pub(super) const MAX_HISTORY: usize = 2000;
pub(super) const INPUT_TTL: u64 = 600;

pub(super) struct Row {
    pub(super) seq: u64,
    pub(super) id: String,
    pub(super) thread: String,
    pub(super) role: &'static str,
    pub(super) text: String,
    pub(super) name: Option<String>,
    pub(super) session: String,
    pub(super) revision: u64,
    pub(super) time: u64,
    pub(super) status: String,
    pub(super) reason: Option<String>,
    pub(super) language: Option<String>,
    pub(super) offline: Option<Value>,
    pub(super) payload: Option<Value>,
    pub(super) queued_at: u64,
    pub(super) attempts: usize,
    pub(super) next_attempt: u64,
    pub(super) pull_claimed_by: Option<String>,
}
impl Row {
    fn view(&self) -> Value {
        json!({"seq": self.seq, "id": self.id, "thread": self.thread, "role": self.role,
        "text": self.text, "name": self.name, "session": self.session, "revision": self.revision,
        "time": self.time, "status": self.status, "audio_reason": self.reason, "offline": self.offline})
    }
}

impl Room {
    pub fn history(&self, thread: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("room lock");
        json!({"messages":inner.rows.iter().filter(|r|thread.is_none_or(|t|r.thread==t)).rev().take(1000).collect::<Vec<_>>().into_iter().rev().map(Row::view).collect::<Vec<_>>()})
    }
    pub fn reply_language(&self, row_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("room lock")
            .rows
            .iter()
            .find(|r| r.id == row_id)
            .and_then(|r| r.language.clone())
    }
}
pub(super) fn trim_rows(inner: &mut Inner) {
    while inner.rows.len() > MAX_HISTORY
        && inner
            .rows
            .front()
            .is_some_and(|r| !matches!(r.status.as_str(), "pending" | "sending"))
    {
        if let Some(row) = inner.rows.pop_front() {
            inner.utterances.retain(|_, u| u.row_id != row.id);
        }
    }
}
pub(super) fn input_receipt(
    inner: &Inner,
    session: &str,
    history_id: &str,
    thread: &str,
    revision: u64,
    status: &str,
) {
    if let Some(browser) = inner.browsers.get(session) {
        let _ = browser.sender.try_send(
            json!({"type":"voice-input-receipt","data":{"revision":revision,
            "history_id":history_id,"thread_id":thread,"session_id":session,"status":status}}),
        );
    }
}
