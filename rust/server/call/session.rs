//! A running call: the loop between the device socket, the call's media and
//! turns, and the room, until either side ends it.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::pipeline::CallFrame;
use crate::runtime::API;
use crate::server::media::{self, CallMedia, TurnOwner};
use crate::server::AppState;
use crate::types::CallSettings;

use super::admission::{admit, await_hello, Admitted};
use super::registration::CallRegistration;
use super::{close, text, UNPAIRED};

mod client_frames;

const MAX_CLIENT_TEXT: usize = 1024 * 1024;
/// How long the loop sleeps when no turn has a deadline.
const IDLE_WAKE: Duration = Duration::from_secs(3600);

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
) {
    let Some(id) = device else {
        let _ = socket.send(close(UNPAIRED, &close_reason)).await;
        return;
    };
    let mut registration = CallRegistration::new(state.clone(), id.clone());
    let (events, output) = mpsc::channel::<Value>(128);
    let control = events.clone();
    let Some(hello) = await_hello(&mut socket, &mut registration, &close_reason).await else {
        return;
    };
    let Some(admitted) = admit(&state, &mut socket, id, &hello, events).await else {
        return;
    };
    let mut call = Call::open(
        state,
        socket,
        registration,
        close_reason,
        admitted,
        control,
        output,
    );
    call.trace_started(&hello);
    call.announce().await;
    call.state
        .welcome(&call.session, hello.get("data"), &call.settings);
    call.serve().await;
    call.close().await;
}

struct Call {
    state: Arc<AppState>,
    socket: WebSocket,
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
}

impl Call {
    /// Registers the admitted call's media, input cancellation and settings on the node.
    fn open(
        state: Arc<AppState>,
        socket: WebSocket,
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
        Self {
            state,
            socket,
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

    async fn announce(&mut self) {
        let room = json!({"api": API, "version": env!("CARGO_PKG_VERSION")});
        let session = json!({"type":"voice-session","data":{"session_id":self.session,"sample_rate":16000,"channels":1,"room":room}});
        let _ = self.socket.send(text(&session)).await;
    }

    async fn serve(&mut self) {
        loop {
            let deadline = self
                .turns
                .deadline()
                .map(tokio::time::Instant::from_std)
                .unwrap_or_else(|| tokio::time::Instant::now() + IDLE_WAKE);
            let flow = tokio::select! {
                _ = self.registration.changed() => {
                    let _ = self.socket.send(close(UNPAIRED, &self.close_reason)).await;
                    Flow::Stop
                }
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
                message = self.socket.recv() => self.on_socket(message).await,
            };
            if matches!(flow, Flow::Stop) {
                break;
            }
        }
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
            && self.socket.send(text(&event)).await.is_err()
        {
            return Flow::Stop;
        }
        Flow::Continue
    }

    /// Forwards a room event to the device; speech is rendered off the loop first.
    async fn on_output(&mut self, event: Value) -> Flow {
        if event.get("type").and_then(Value::as_str) == Some("voice-speech") {
            self.render_speech(event);
            return Flow::Continue;
        }
        if self.socket.send(text(&event)).await.is_err() {
            return Flow::Stop;
        }
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
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Flow::Stop,
            Some(Ok(Message::Ping(bytes))) => {
                let _ = self.socket.send(Message::Pong(bytes)).await;
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
