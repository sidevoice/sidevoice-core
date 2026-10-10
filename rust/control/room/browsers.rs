//! Browser calls: each call's focus and turn, and the room's recent sessions.
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::journal::InputRef;
use super::turns::VoiceTurn;
use super::util::id;

/// How many recent session IDs the room remembers, including calls that already ended.
const RECENT_SESSIONS: usize = 64;
/// How many of a call's turns may wait for their words at once; one more is refused (`room.turns_full`) until one ends.
pub(super) const MAX_OPEN_TURNS: usize = 8;

/// The conversation a call is focused on; an empty thread means it is focused on none.
#[derive(Clone)]
pub(super) struct Target {
    pub(super) thread: String,
    pub(super) title: Option<String>,
    pub(super) binding_id: String,
}
impl Target {
    pub(super) fn none() -> Self {
        Self {
            thread: String::new(),
            title: None,
            binding_id: id(),
        }
    }
    pub(super) fn view(&self) -> Value {
        json!({"thread_id": self.thread, "title": self.title, "binding_id": self.binding_id})
    }
}

pub(super) struct Browser {
    pub(super) device: String,
    pub(super) language: String,
    sender: mpsc::Sender<Value>,
    pub(super) target: Option<Target>,
    pub(super) revision: u64,
    pub(super) turn_revision: u64,
    pub(super) speaking: bool,
    /// The turn the person is speaking now, by revision, if any.
    pub(super) open_turn: Option<u64>,
    /// The turns started and not yet ended, oldest first, each with the focus it started on: its words go there
    /// when they come, even after a newer turn started or the focus moved. At most [`MAX_OPEN_TURNS`].
    pub(super) turns: VecDeque<VoiceTurn>,
    pub(super) sent: u64,
    /// Since when the call has had no socket: nothing new is handed to it until its page is back.
    pub(super) parked: Option<Instant>,
}
impl Browser {
    pub(super) fn new(device: String, language: String, sender: mpsc::Sender<Value>) -> Self {
        Self {
            device,
            language,
            sender,
            target: None,
            revision: 0,
            turn_revision: 0,
            speaking: false,
            open_turn: None,
            turns: VecDeque::new(),
            sent: 0,
            parked: None,
        }
    }
    /// Send an event to the browser if its channel has room; false if it was dropped.
    pub(super) fn offer(&self, event: Value) -> bool {
        self.sender.try_send(event).is_ok()
    }
    /// Send an event the browser may miss without harm.
    pub(super) fn notify(&self, event: Value) {
        self.offer(event);
    }
    /// Start a new revision: what the call is sent from now on belongs to it.
    pub(super) fn next_revision(&mut self) -> u64 {
        self.revision += 1;
        self.revision
    }
    /// Move the call to another focus under a new revision. The person is no longer speaking to the old focus; the
    /// words of a turn already started still go there when they come.
    pub(super) fn refocus(&mut self, target: Target) {
        self.next_revision();
        self.speaking = false;
        self.open_turn = None;
        self.target = Some(target);
    }
    pub(super) fn is_on(&self, thread: &str) -> bool {
        self.target.as_ref().is_some_and(|t| t.thread == thread)
    }
    /// The focus, if the call is focused on a conversation.
    pub(super) fn bound_target(&self) -> Option<&Target> {
        self.target.as_ref().filter(|t| !t.thread.is_empty())
    }
}

#[derive(Default)]
pub(super) struct Browsers {
    calls: HashMap<String, Browser>,
    recent: VecDeque<String>,
}
impl Browsers {
    pub(super) fn join(&mut self, sid: &str, browser: Browser) {
        self.recent.push_back(sid.to_owned());
        if self.recent.len() > RECENT_SESSIONS {
            self.recent.pop_front();
        }
        self.calls.insert(sid.to_owned(), browser);
    }
    pub(super) fn leave(&mut self, sid: &str) {
        self.calls.remove(sid);
    }
    pub(super) fn get(&self, sid: &str) -> Option<&Browser> {
        self.calls.get(sid)
    }
    pub(super) fn get_mut(&mut self, sid: &str) -> Option<&mut Browser> {
        self.calls.get_mut(sid)
    }
    pub(super) fn contains(&self, sid: &str) -> bool {
        self.calls.contains_key(sid)
    }
    pub(super) fn len(&self) -> usize {
        self.calls.len()
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = (&String, &Browser)> {
        self.calls.iter()
    }
    /// Whether `sid` is one of the recent sessions, ended or not.
    pub(super) fn is_recent(&self, sid: &str) -> bool {
        self.recent.iter().any(|s| s == sid)
    }
    /// The most recent session among `candidates`.
    pub(super) fn most_recent_of(&self, candidates: &[String]) -> Option<&String> {
        self.recent
            .iter()
            .rev()
            .find(|candidate| candidates.contains(candidate))
    }
    /// The calls focused on `thread`.
    pub(super) fn on_thread<'a>(
        &'a self,
        thread: &'a str,
    ) -> impl Iterator<Item = (&'a String, &'a Browser)> {
        self.calls.iter().filter(move |(_, b)| b.is_on(thread))
    }
    pub(super) fn ids_on_thread(&self, thread: &str) -> Vec<String> {
        self.on_thread(thread).map(|(sid, _)| sid.clone()).collect()
    }
    /// Tell the call that sent an input row what became of it.
    pub(super) fn input_receipt(&self, input: &InputRef, status: &str) {
        if let Some(browser) = self.calls.get(&input.session) {
            if let Some(telemetry) = crate::control::telemetry::shared() {
                telemetry.receipt(&input.session, status, input.thread.as_deref());
            }
            let mut data = json!({"revision":input.revision,"history_id":input.id,"thread_id":input.thread,
                "session_id":input.session,"status":status});
            if let Some(turn_id) = &input.turn_id {
                data["turn_id"] = json!(turn_id);
            }
            browser.notify(json!({"type":"voice-input-receipt","data":data}));
        }
    }
}
