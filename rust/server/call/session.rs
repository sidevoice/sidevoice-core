//! A running call: the loop between the device socket, the call's media and
//! turns, and the room, until either side ends it. A socket that goes without a hang-up
//! parks the call for its page to come back on another one.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::pipeline::CallFrame;
use crate::runtime::API;
use crate::server::media::{self, CallMedia, TurnOwner};
use crate::server::AppState;
use crate::types::CallSettings;

use super::admission::{admit, await_hello, refuse_full, Admitted};
use super::heartbeat::{Beat, Heartbeat};
use super::registration::CallRegistration;
use super::resume::{new_token, resume_window, Outbound, Reattach};
use super::{close, UNPAIRED};

mod client_frames;

const MAX_CLIENT_TEXT: usize = 1024 * 1024;
/// How long the loop sleeps when no turn has a deadline.
const IDLE_WAKE: Duration = Duration::from_secs(3600);
/// The close code a page hangs up with; any other end of the socket parks the call.
const HANG_UP: u16 = 1000;

/// A rendered speech event: its utterance and revision, and the event to send, if any.
type Rendered = (String, u64, Option<Value>);

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
    let control = events.clone();
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
        control,
        output,
    );
    call.trace_started(&hello);
    call.announce(refused).await;
    call.state
        .welcome(&call.session, hello.get("data"), &call.settings);
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
    defaults: CallSettings,
    /// What the call speaks with; later settings replace only some of it.
    settings: CallSettings,
    session: String,
    media: Arc<CallMedia>,
    turns: TurnOwner,
    detector_events: mpsc::Receiver<CallFrame>,
    focus_events: mpsc::Receiver<()>,
    cancelled: mpsc::Receiver<u64>,
    output: mpsc::Receiver<Value>,
    rendered_tx: mpsc::Sender<Rendered>,
    rendered: mpsc::Receiver<Rendered>,
    /// What the hello got wrong, sent right after the session.
    problems: Vec<Value>,
    /// What the room shows about this call's transcription; `voice-stt-ready` adds to it.
    transcription: Value,
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
    /// Registers the admitted call's media, input cancellation and settings on the node.
    #[expect(
        clippy::too_many_arguments,
        reason = "the call takes over everything admission set up"
    )]
    fn open(
        state: Arc<AppState>,
        socket: WebSocket,
        device: String,
        registration: CallRegistration,
        close_reason: String,
        admitted: Admitted,
        control: mpsc::Sender<Value>,
        output: mpsc::Receiver<Value>,
    ) -> Self {
        let Admitted {
            defaults,
            settings,
            session,
            media,
            detector_events,
            focus_events,
            problems,
            transcription,
        } = admitted;
        state
            .media
            .lock()
            .expect("media lock")
            .insert(session.clone(), media.clone());
        let turns = TurnOwner::new(
            media.clone(),
            state.room.clone(),
            settings.clone(),
            session.clone(),
            control,
            state.dir.clone(),
        );
        let (cancel_tx, cancelled) = mpsc::channel::<u64>(8);
        state
            .cancel_input
            .lock()
            .expect("cancel input lock")
            .insert(session.clone(), cancel_tx);
        state
            .call_settings
            .lock()
            .expect("call settings lock")
            .insert(session.clone(), settings.clone());
        let (rendered_tx, rendered) = mpsc::channel::<Rendered>(16);
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
            defaults,
            settings,
            session,
            media,
            turns,
            detector_events,
            focus_events,
            cancelled,
            output,
            rendered_tx,
            rendered,
            problems,
            transcription,
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
            let stt = &self.settings.stt;
            telemetry.call_started(
                &self.session,
                hello["data"]["telemetry"]["traceparent"].as_str(),
                &json!({"sidevoice.stt_place": stt.place, "sidevoice.stt_model": stt.model,
                    "sidevoice.stt_accelerator": stt.build.as_ref().map(|build| &build.accelerator),
                    "sidevoice.turn_end_mode": self.settings.turn_end_mode}),
            );
        }
    }

    /// The session frame: where the call is, and the token that brings the page back to it.
    fn session_frame(&self, resumed: Value) -> Value {
        let room = json!({"api": API, "version": env!("CARGO_PKG_VERSION")});
        let mut data = json!({"session_id":self.session,"sample_rate":16000,"channels":1,"room":room,
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
        let Some(missed) = self.outbound.since(last_seq) else {
            let _ = answer.send(Err((socket, "gap")));
            return Flow::Stop;
        };
        if answer.send(Ok(())).is_err() {
            return Flow::Continue;
        }
        // Nothing the person did not hear is played late: speech the page never got is not sent again,
        // and the room marks it unheard together with whatever was published while the page was away.
        let (speech, missed): (Vec<_>, Vec<_>) = missed.into_iter().partition(|(kind, _, _)| {
            matches!(
                kind.as_str(),
                "voice-speech" | "voice-speech-audio" | "voice-replay"
            )
        });
        let unreceived: Vec<String> = speech.into_iter().filter_map(|(_, uid, _)| uid).collect();
        self.socket = Some(socket);
        self.parked_until = None;
        self.last_frame = tokio::time::Instant::now();
        self.state.room.resume(&self.session, &unreceived);
        self.token = new_token();
        self.state.resumable.renew(&self.session, &self.token);
        let session = self.session_frame(json!({"resumed": true}));
        self.deliver(session.to_string()).await;
        for (_, _, frame) in missed {
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
            let deadline = self
                .turns
                .deadline()
                .map(tokio::time::Instant::from_std)
                .unwrap_or_else(|| tokio::time::Instant::now() + IDLE_WAKE);
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
                frame = self.detector_events.recv() => match frame {
                    Some(frame) => {
                        self.on_detector_frame(frame).await;
                        Flow::Continue
                    }
                    None => Flow::Stop,
                },
                focus = self.focus_events.recv() => {
                    if focus.is_some() {
                        self.turns.focus_changed().await;
                        self.state.prune_replay_audio();
                    }
                    Flow::Continue
                }
                result = self.turns.finished.recv() => {
                    if let Some(done) = result {
                        self.turns.result(done).await;
                        self.state.prune_replay_audio();
                    }
                    Flow::Continue
                }
                cancelled = self.cancelled.recv() => {
                    if let Some(revision) = cancelled {
                        self.turns.cancel(revision).await;
                        self.state.prune_replay_audio();
                    }
                    Flow::Continue
                }
                _ = tokio::time::sleep_until(deadline) => {
                    self.turns.expired().await;
                    Flow::Continue
                }
                rendered = self.rendered.recv() => match rendered {
                    Some(rendered) => self.on_rendered(rendered).await,
                    None => Flow::Continue,
                },
                event = self.output.recv() => match event {
                    Some(event) => self.on_output(event).await,
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

    async fn on_detector_frame(&mut self, frame: CallFrame) {
        let started = matches!(&frame, CallFrame::Started);
        self.turns.frame(frame).await;
        if started {
            self.state.prune_replay_audio();
        }
    }

    /// Sends a rendered speech event if it is still current, or fails its receipt.
    async fn on_rendered(&mut self, (uid, revision, event): Rendered) -> Flow {
        let Some(event) = event else {
            let _ = self
                .state
                .room
                .receipt(&self.session, &uid, revision, "failed");
            self.state.prune_replay_audio();
            return Flow::Continue;
        };
        if self
            .state
            .room
            .speech_current(&self.session, &uid, revision)
        {
            self.send(event).await;
        }
        Flow::Continue
    }

    /// Forwards a room event to the device; speech is rendered off the loop first.
    async fn on_output(&mut self, event: Value) -> Flow {
        if event.get("type").and_then(Value::as_str) == Some("voice-speech") {
            self.render_speech(event);
            return Flow::Continue;
        }
        self.send(event).await;
        Flow::Continue
    }

    fn render_speech(&self, event: Value) {
        let uid = event["data"]["utterance_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let revision = event["data"]["revision"].as_u64().unwrap_or(0);
        let replay_audio = self
            .state
            .replay_audio
            .lock()
            .expect("replay audio lock")
            .get(&uid)
            .cloned();
        let room = self.state.room.clone();
        let dir = self.state.dir.clone();
        let cache = self.state.synthesis.clone();
        let settings = self.settings.clone();
        let sid = self.session.clone();
        let rendered = self.rendered_tx.clone();
        tokio::spawn(async move {
            let result =
                media::speech_event(room, &sid, &settings, &dir, cache, event, replay_audio).await;
            let _ = rendered.send((uid, revision, result)).await;
        });
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
            Some(Ok(Message::Binary(pcm))) => {
                self.media.feed(media::Source::Socket, pcm.to_vec()).await;
            }
            Some(Ok(Message::Text(raw))) if raw.len() <= MAX_CLIENT_TEXT => {
                self.on_client_text(&raw).await;
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Releases everything `open` registered, then leaves the room.
    async fn close(mut self) {
        self.state.resumable.close(&self.session);
        self.reattach.close();
        while let Ok(attach) = self.reattach.try_recv() {
            let _ = attach.answer.send(Err((attach.socket, "unknown")));
        }
        self.turns.close().await;
        self.state
            .cancel_input
            .lock()
            .expect("cancel input lock")
            .remove(&self.session);
        self.state
            .call_settings
            .lock()
            .expect("call settings lock")
            .remove(&self.session);
        self.media.close();
        self.media.close_rtc().await;
        self.state
            .media
            .lock()
            .expect("media lock")
            .remove(&self.session);
        self.state.retire_session_replays(&self.session);
    }
}
