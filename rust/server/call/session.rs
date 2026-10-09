//! A running call: the loop between the device socket and the room, until either side ends it. A socket that goes
//! without a hang-up parks the call for its page to come back on another one.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::runtime::API;
use crate::server::AppState;

use super::admission::{admit, await_hello, refuse_full, Admitted};
use super::heartbeat::{Beat, Heartbeat};
use super::registration::CallRegistration;
use super::resume::{missed_on_return, new_token, resume_window, Missed, Outbound, Reattach};
use super::{close, UNPAIRED};

mod client_frames;

const MAX_CLIENT_TEXT: usize = 1024 * 1024;
/// How long the loop sleeps when nothing has a deadline.
const IDLE_WAKE: Duration = Duration::from_secs(3600);
/// The close code a page hangs up with; any other end of the socket parks the call.
const HANG_UP: u16 = 1000;

enum Flow {
    Continue,
    Stop,
}

pub(super) async fn run(
    state: Arc<AppState>,
    device: Option<String>,
    mut socket: WebSocket,
    close_reason: String,
    language: String,
) {
    let Some(id) = device else {
        let _ = socket.send(close(UNPAIRED, &close_reason)).await;
        return;
    };
    let mut registration = CallRegistration::new(state.clone(), id.clone());
    let Some(hello) = await_hello(&mut socket, &mut registration, &close_reason).await else {
        return;
    };
    let mut refused = None;
    if let Some(asked) = hello.get("data").and_then(|data| data.get("resume")) {
        match reattach(&state, &id, socket, asked).await {
            None => return,
            Some((back, reason)) => {
                socket = back;
                refused = Some(reason);
            }
        }
    }
    // A full room is said right after the hello: a call coming back to its seat never meets it.
    let admission = state.room.admission(&language);
    if admission["admitted"] == Value::Bool(false) {
        refuse_full(&mut socket, &admission).await;
        return;
    }
    let (events, output) = mpsc::channel::<Value>(128);
    let Some(admitted) = admit(&state, &mut socket, id.clone(), &hello, events).await else {
        return;
    };
    let mut call = Call::open(
        state,
        socket,
        id,
        registration,
        close_reason,
        admitted,
        output,
    );
    call.trace_started(&hello);
    call.announce(refused).await;
    call.welcome(hello.get("data"));
    call.serve().await;
    call.close().await;
}

/// Hands `socket` to the parked or live call `asked` names, if its device holds that call's token.
/// `None` once the call has the socket; otherwise the socket back, with why the page starts anew.
async fn reattach(
    state: &AppState,
    device: &str,
    socket: WebSocket,
    asked: &Value,
) -> Option<(WebSocket, &'static str)> {
    let session = asked["session_id"].as_str().unwrap_or("");
    let token = asked["token"].as_str().unwrap_or("");
    let Some(attach) = state.resumable.take(session, device, token) else {
        return Some((socket, "unknown"));
    };
    let (answer, answered) = oneshot::channel();
    let request = Reattach {
        socket,
        last_seq: asked["last_seq"].as_u64().unwrap_or(0),
        answer,
    };
    if let Err(mpsc::error::SendError(request)) = attach.send(request).await {
        return Some((request.socket, "unknown"));
    }
    answered.await.ok()?.err()
}

/// The socket's next message, for a call that has one.
async fn next_message(socket: &mut Option<WebSocket>) -> Option<Result<Message, axum::Error>> {
    match socket.as_mut() {
        Some(socket) => socket.recv().await,
        None => std::future::pending().await,
    }
}

struct Call {
    state: Arc<AppState>,
    /// The page's socket; none while the call is parked.
    socket: Option<WebSocket>,
    device: String,
    registration: CallRegistration,
    close_reason: String,
    /// The language of the call's interface and of what the room renders for it.
    language: String,
    session: String,
    output: mpsc::Receiver<Value>,
    /// What the hello got wrong, sent right after the session.
    problems: Vec<Value>,
    /// The browser keepalive, and when anything last arrived on the socket.
    keepalive: Option<Heartbeat>,
    last_frame: tokio::time::Instant,
    beats: tokio::time::Interval,
    /// What was sent, numbered, for a page that comes back.
    outbound: Outbound,
    /// The token that lets the page take this call over again, once.
    token: String,
    resume_window: Duration,
    parked_until: Option<tokio::time::Instant>,
    reattach: mpsc::Receiver<Reattach>,
}

impl Call {
    /// Makes the admitted call one a page may come back to.
    fn open(
        state: Arc<AppState>,
        socket: WebSocket,
        device: String,
        registration: CallRegistration,
        close_reason: String,
        admitted: Admitted,
        output: mpsc::Receiver<Value>,
    ) -> Self {
        let Admitted {
            session,
            language,
            problems,
        } = admitted;
        let keepalive = Heartbeat::from_env();
        let every = keepalive.map_or(IDLE_WAKE, |beat| beat.interval);
        let mut beats = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        beats.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let token = new_token();
        let (attach, reattach) = mpsc::channel(1);
        state.resumable.open(&session, &device, &token, attach);
        Self {
            state,
            socket: Some(socket),
            device,
            registration,
            close_reason,
            language,
            session,
            output,
            problems,
            keepalive,
            last_frame: tokio::time::Instant::now(),
            beats,
            outbound: Outbound::default(),
            token,
            resume_window: resume_window(std::env::var("VOICE_RESUME_SECONDS").ok().as_deref()),
            parked_until: None,
            reattach,
        }
    }

    /// The hello carries the browser's call span: the room's turns go inside the browser's call.
    fn trace_started(&self, hello: &Value) {
        if let Some(telemetry) = crate::control::telemetry::shared() {
            telemetry.call_started(
                &self.session,
                hello["data"]["telemetry"]["traceparent"].as_str(),
                &json!({}),
            );
        }
    }

    /// A call that just joined is focused on the conversation its hello names, if that one is still in the room.
    fn welcome(&self, hello: Option<&Value>) {
        if let Some(thread) = hello
            .and_then(|data| data.get("conversation"))
            .and_then(Value::as_str)
        {
            self.state.room.restore_focus(&self.session, thread);
        }
    }

    /// The session frame: where the call is, and the token that brings the page back to it.
    fn session_frame(&self, resumed: Value) -> Value {
        let room = json!({"api": API, "version": env!("CARGO_PKG_VERSION")});
        let mut data = json!({"session_id":self.session,"room":room,
            "resume":{"token":self.token,"seconds":self.resume_window.as_secs_f64()}});
        if let (Some(data), Some(resumed)) = (data.as_object_mut(), resumed.as_object()) {
            data.extend(resumed.clone());
        }
        json!({"type":"voice-session","data":data})
    }

    async fn announce(&mut self, refused: Option<&str>) {
        let mut resumed = json!({"resumed": false});
        if let Some(reason) = refused {
            resumed["resume_refused"] = json!(reason);
        }
        let session = self.session_frame(resumed);
        self.deliver(session.to_string()).await;
        // Only now can anything reach the browser: what its hello got wrong goes right after the session.
        for problem in std::mem::take(&mut self.problems) {
            self.send(problem).await;
        }
    }

    /// Numbers and keeps a frame for the page, and sends it if the page is there.
    pub(super) async fn send(&mut self, event: Value) {
        let text = self.outbound.stamp(event);
        self.deliver(text).await;
    }

    async fn deliver(&mut self, text: String) {
        let Some(socket) = self.socket.as_mut() else {
            return;
        };
        if socket.send(Message::Text(text.into())).await.is_err() {
            self.park();
        }
    }

    /// The socket is gone without a hang-up: the call keeps running for its page to come back.
    fn park(&mut self) {
        self.socket = None;
        if self.parked_until.is_none() {
            self.parked_until = Some(tokio::time::Instant::now() + self.resume_window);
            self.state.room.park(&self.session, true);
        }
    }

    /// The page is back on `socket`: what it missed follows the session, in order.
    async fn on_reattach(&mut self, attach: Reattach) -> Flow {
        let Reattach {
            socket,
            last_seq,
            answer,
        } = attach;
        let Some(Missed { unreceived, frames }) =
            missed_on_return(&mut self.outbound, &mut self.output, last_seq)
        else {
            let _ = answer.send(Err((socket, "gap")));
            return Flow::Stop;
        };
        if answer.send(Ok(())).is_err() {
            return Flow::Continue;
        }
        self.socket = Some(socket);
        self.parked_until = None;
        self.last_frame = tokio::time::Instant::now();
        self.state.room.resume(&self.session, &unreceived);
        self.token = new_token();
        self.state.resumable.renew(&self.session, &self.token);
        let session = self.session_frame(json!({"resumed": true}));
        self.deliver(session.to_string()).await;
        for frame in frames {
            self.deliver(frame).await;
        }
        Flow::Continue
    }

    /// Tells the page a message it sent has been taken, so it can stop keeping it.
    pub(super) async fn ack(&mut self, id: &str) {
        let ack = json!({"type":"voice-ack","data":{"session_id":self.session,"client_msg_id":id}});
        self.send(ack).await;
    }

    async fn serve(&mut self) {
        loop {
            let parked_until = self
                .parked_until
                .unwrap_or_else(|| tokio::time::Instant::now() + IDLE_WAKE);
            let flow = tokio::select! {
                _ = self.registration.changed() => {
                    if let Some(socket) = self.socket.as_mut() {
                        let _ = socket.send(close(UNPAIRED, &self.close_reason)).await;
                    }
                    Flow::Stop
                }
                // Nobody came back in time: the call ends as if its socket had just closed.
                _ = tokio::time::sleep_until(parked_until), if self.parked_until.is_some() => Flow::Stop,
                attach = self.reattach.recv() => match attach {
                    Some(attach) => self.on_reattach(attach).await,
                    None => Flow::Continue,
                },
                event = self.output.recv() => match event {
                    Some(event) => {
                        self.send(event).await;
                        Flow::Continue
                    }
                    None => Flow::Stop,
                },
                // A browser that stopped answering is treated exactly as if its socket had closed: behind a
                // proxy a closed tab never closes the socket, and its seat is given back when the park ends.
                _ = self.beats.tick(), if self.keepalive.is_some() && self.socket.is_some() => self.on_heartbeat().await,
                message = next_message(&mut self.socket), if self.socket.is_some() => {
                    self.last_frame = tokio::time::Instant::now();
                    self.on_socket(message).await
                }
            };
            if matches!(flow, Flow::Stop) {
                break;
            }
        }
    }

    /// Asks a quiet browser to answer, or parks the call of one that never did.
    async fn on_heartbeat(&mut self) -> Flow {
        match self
            .keepalive
            .map(|beat| beat.check(self.last_frame.elapsed()))
        {
            Some(Beat::Drop) => self.park(),
            Some(Beat::Ask) => {
                let ping = json!({"type":"voice-ping","data":{"session_id":self.session}});
                self.send(ping).await;
            }
            _ => {}
        }
        Flow::Continue
    }

    async fn on_socket(&mut self, message: Option<Result<Message, axum::Error>>) -> Flow {
        match message {
            Some(Ok(Message::Close(Some(frame)))) if frame.code == HANG_UP => return Flow::Stop,
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => self.park(),
            Some(Ok(Message::Ping(bytes))) => {
                if let Some(socket) = self.socket.as_mut() {
                    let _ = socket.send(Message::Pong(bytes)).await;
                }
            }
            Some(Ok(Message::Text(raw))) if raw.len() <= MAX_CLIENT_TEXT => {
                self.on_client_text(&raw).await;
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Gives up the call's way back, then leaves the room.
    async fn close(mut self) {
        self.state.resumable.close(&self.session);
        self.reattach.close();
        while let Ok(attach) = self.reattach.try_recv() {
            let _ = attach.answer.send(Err((attach.socket, "unknown")));
        }
        self.state.room.leave(&self.session);
    }
}
