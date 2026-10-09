//! What lets a dropped call come back as itself: its single-use resume token, the frames it sent
//! numbered so a returning page gets exactly what it missed, and the client messages already taken.
//!
//! A socket that dies does not end the call. The call parks: its turns, recognition and seat stay, and
//! whatever it would have said waits in the ring. A page that comes back with the session's token and
//! the last `seq` it handled takes the call over on its new socket and gets every later frame again.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::ws::WebSocket;
use serde_json::Value;
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, oneshot};

/// How long a call whose socket went stays parked for its page to come back.
const RESUME_SECONDS: f64 = 60.0;
/// What a call keeps of what it sent, by frames and by bytes, for a page that comes back.
const RING_FRAMES: usize = 512;
const RING_BYTES: usize = 16 * 1024 * 1024;
/// How many client message ids are remembered per device.
const SEEN_PER_DEVICE: usize = 256;

/// `VOICE_RESUME_SECONDS` as the core reads it: unreadable or negative is the default.
pub(super) fn resume_window(value: Option<&str>) -> Duration {
    let seconds = value
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .unwrap_or(RESUME_SECONDS);
    Duration::from_secs_f64(seconds)
}

pub(super) fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A returning page handing its new socket to the call it left.
pub(in crate::server) struct Reattach {
    pub(in crate::server) socket: WebSocket,
    pub(in crate::server) last_seq: u64,
    /// The socket comes back, with why, when the call cannot give the page what it missed.
    pub(in crate::server) answer: oneshot::Sender<Result<(), (WebSocket, &'static str)>>,
}

struct Resumable {
    device: String,
    token: String,
    attach: mpsc::Sender<Reattach>,
}

/// Every live call by session, with the token that may take it over.
#[derive(Default)]
pub(in crate::server) struct ResumableCalls {
    calls: Mutex<HashMap<String, Resumable>>,
}

impl ResumableCalls {
    pub(in crate::server) fn open(
        &self,
        session: &str,
        device: &str,
        token: &str,
        attach: mpsc::Sender<Reattach>,
    ) {
        self.lock().insert(
            session.to_owned(),
            Resumable {
                device: device.to_owned(),
                token: token.to_owned(),
                attach,
            },
        );
    }

    /// The way into `session` for its own device holding its current token, which is spent.
    pub(super) fn take(
        &self,
        session: &str,
        device: &str,
        token: &str,
    ) -> Option<mpsc::Sender<Reattach>> {
        let mut calls = self.lock();
        let call = calls.get_mut(session)?;
        let matches: bool = call.token.as_bytes().ct_eq(token.as_bytes()).into();
        if call.device != device || call.token.is_empty() || !matches {
            return None;
        }
        call.token.clear();
        Some(call.attach.clone())
    }

    pub(super) fn renew(&self, session: &str, token: &str) {
        if let Some(call) = self.lock().get_mut(session) {
            call.token = token.to_owned();
        }
    }

    pub(super) fn close(&self, session: &str) {
        self.lock().remove(session);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Resumable>> {
        self.calls.lock().expect("resumable calls lock")
    }
}

/// The frames a call sent, numbered from 1, the newest kept within the ring's bounds.
#[derive(Default)]
pub(super) struct Outbound {
    seq: u64,
    frames: VecDeque<Sent>,
    bytes: usize,
}

impl Outbound {
    /// Numbers `event` and keeps it; the text to send.
    pub(super) fn stamp(&mut self, mut event: Value) -> String {
        self.seq += 1;
        if let Some(object) = event.as_object_mut() {
            object.insert("seq".into(), self.seq.into());
        }
        let text = event.to_string();
        self.bytes += text.len();
        self.frames.push_back(Sent {
            seq: self.seq,
            kind: event["type"].as_str().unwrap_or("").to_owned(),
            utterance: event["data"]["utterance_id"].as_str().map(str::to_owned),
            text: text.clone(),
        });
        while self.frames.len() > RING_FRAMES || (self.bytes > RING_BYTES && self.frames.len() > 1)
        {
            if let Some(old) = self.frames.pop_front() {
                self.bytes -= old.text.len();
            }
        }
        text
    }

    /// Every frame after `last_seq`, as its type, the utterance it carries if any, and its text; or
    /// none at all when some of them are no longer kept.
    pub(super) fn since(&self, last_seq: u64) -> Option<Vec<(String, Option<String>, String)>> {
        if last_seq >= self.seq {
            return Some(Vec::new());
        }
        let oldest = self.frames.front().map_or(self.seq + 1, |frame| frame.seq);
        if oldest > last_seq + 1 {
            return None;
        }
        Some(
            self.frames
                .iter()
                .filter(|frame| frame.seq > last_seq)
                .map(|frame| {
                    (
                        frame.kind.clone(),
                        frame.utterance.clone(),
                        frame.text.clone(),
                    )
                })
                .collect(),
        )
    }
}

/// What a returning page is given after the session: the frames it missed, and the replies it never received.
pub(super) struct Missed {
    /// The utterances of replies sent toward the page that never reached it: the room marks them unheard.
    pub(super) unreceived: Vec<String>,
    /// Every other frame after the page's `last_seq`, in order.
    pub(super) frames: Vec<String>,
}

/// What a page that handled every frame up to `last_seq` missed; none when some of it is no longer kept. What the
/// room had already queued for the call (`queued`) and the call had not sent yet is taken first, numbered with the
/// rest: a reply still on its way when the socket went counts as unreceived like one sent into the dead socket.
/// Nothing the person did not hear is played late, so no reply is sent again.
pub(super) fn missed_on_return(
    outbound: &mut Outbound,
    queued: &mut mpsc::Receiver<Value>,
    last_seq: u64,
) -> Option<Missed> {
    while let Ok(event) = queued.try_recv() {
        outbound.stamp(event);
    }
    let (replies, frames): (Vec<_>, Vec<_>) = outbound
        .since(last_seq)?
        .into_iter()
        .partition(|(kind, _, _)| kind == "voice-reply");
    Some(Missed {
        unreceived: replies.into_iter().filter_map(|(_, uid, _)| uid).collect(),
        frames: frames.into_iter().map(|(_, _, text)| text).collect(),
    })
}

struct Sent {
    seq: u64,
    kind: String,
    utterance: Option<String>,
    text: String,
}

/// The client messages each device already had taken, the newest [`SEEN_PER_DEVICE`] of them.
#[derive(Default)]
pub(in crate::server) struct SeenMessages {
    devices: Mutex<HashMap<String, VecDeque<String>>>,
}

impl SeenMessages {
    /// Claims message `id` of `device` for whoever takes it: true the first time, false for a repeat, under one lock,
    /// so two calls of one device sending the same message at once take it once between them.
    pub(in crate::server) fn claim(&self, device: &str, id: &str) -> bool {
        let mut devices = self.devices.lock().expect("seen messages lock");
        let seen = devices.entry(device.to_owned()).or_default();
        if seen.iter().any(|known| known == id) {
            return false;
        }
        seen.push_back(id.to_owned());
        while seen.len() > SEEN_PER_DEVICE {
            seen.pop_front();
        }
        true
    }
}

/// A client message's id, when it carries a usable one.
pub(in crate::server) fn client_msg_id(data: &Value) -> Option<&str> {
    data.get("client_msg_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 64)
}

#[cfg(test)]
mod tests;
