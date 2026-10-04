//! One microphone source and one turn lifecycle for an authenticated call.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use base64::Engine;
use rustvani::audio_process::resamplers::{ResamplerQuality, StreamResampler};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use unicode_script::{Script, UnicodeScript};
use uuid::Uuid;

use crate::{
    control::room::{latency_now_micros, LatencyEvent, Room, VoiceTurn},
    models::resolve_voice,
    pipeline::{CallDetector, CallFrame},
    providers::{
        cache::{SynthesisCache, SynthesisChoice},
        ElevenLabsTts, OpenAiTranscriber,
    },
    storage::PrivateDir,
    types::CallSettings,
};

const MAX_TURN_BYTES: usize = 16_000 * 2 * 60;
const PRE_ROLL_BYTES: usize = 16_000 * 2;
const MAX_RECOGNITION_QUEUE: usize = 8;
const CATCHUP_SLICE_BYTES: usize = 128 * 1024;
const CATCHUP_MAX_SECONDS: usize = 35;

#[derive(Debug)]
pub enum SttFailure {
    Timeout,
    Device,
    Provider,
}
type Recognition = Result<Option<String>, SttFailure>;
type PendingTranscripts = HashMap<String, oneshot::Sender<Result<Option<String>, ()>>>;

fn device_timeout() -> Duration {
    #[cfg(feature = "hosted-fixtures")]
    if let Some(milliseconds) = std::env::var("SIDEVOICE_FIXTURE_STT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (100..=90_000).contains(value))
    {
        return Duration::from_millis(milliseconds);
    }
    Duration::from_secs(90)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Socket,
    WebRtc,
}

pub struct CallMedia {
    detector: CallDetector,
    source: Mutex<Source>,
    transcripts: Mutex<PendingTranscripts>,
    rtc_generation: AtomicU64,
    rtc: Mutex<Option<Arc<dyn webrtc::peer_connection::PeerConnection>>>,
    focus_tx: mpsc::Sender<()>,
    playing_uid: Mutex<Option<String>>,
}

type MediaStart = (
    Arc<CallMedia>,
    mpsc::Receiver<CallFrame>,
    mpsc::Receiver<()>,
);

impl CallMedia {
    pub fn start(settings: &CallSettings) -> Result<MediaStart, String> {
        let (detector, events) = CallDetector::start(settings)?;
        let (focus_tx, focus_rx) = mpsc::channel(8);
        Ok((
            Arc::new(Self {
                detector,
                source: Mutex::new(Source::Socket),
                transcripts: Mutex::new(HashMap::new()),
                rtc_generation: AtomicU64::new(0),
                rtc: Mutex::new(None),
                focus_tx,
                playing_uid: Mutex::new(None),
            }),
            events,
            focus_rx,
        ))
    }

    pub fn select(&self, source: Source) {
        *self.source.lock().expect("source lock") = source;
    }
    pub async fn focus_changed(&self) {
        let _ = self.focus_tx.send(()).await;
    }

    pub async fn feed(&self, source: Source, pcm: Vec<u8>) {
        if *self.source.lock().expect("source lock") == source {
            let _ = self.detector.feed(pcm).await;
        }
    }

    pub async fn feed_rtc(&self, generation: u64, pcm: Vec<u8>) {
        if self.rtc_generation.load(Ordering::Acquire) == generation {
            self.feed(Source::WebRtc, pcm).await;
        }
    }

    pub async fn replace_rtc(&self) -> u64 {
        let generation = self.rtc_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let prior = self.rtc.lock().expect("rtc lock").take();
        if let Some(prior) = prior {
            let _ = prior.close().await;
        }
        generation
    }

    pub async fn set_rtc(
        &self,
        generation: u64,
        peer: Arc<dyn webrtc::peer_connection::PeerConnection>,
    ) {
        if self.rtc_generation.load(Ordering::Acquire) == generation {
            self.rtc.lock().expect("rtc lock").replace(peer);
        } else {
            let _ = peer.close().await;
        }
    }

    pub async fn close_rtc(&self) {
        self.rtc_generation.fetch_add(1, Ordering::AcqRel);
        let peer = self.rtc.lock().expect("rtc lock").take();
        if let Some(peer) = peer {
            let _ = peer.close().await;
        }
    }

    pub fn transcript(&self, data: &Value, error: bool, session: &str) {
        if data.get("session_id").and_then(Value::as_str) != Some(session) {
            return;
        }
        let Some(request) = data.get("request_id").and_then(Value::as_str) else {
            return;
        };
        let pending = self
            .transcripts
            .lock()
            .expect("transcripts lock")
            .remove(request);
        if let Some(pending) = pending {
            let text = if error {
                Err(())
            } else {
                Ok(data
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned))
            };
            let _ = pending.send(text);
        }
    }

    async fn recognize(
        &self,
        pcm: Vec<u8>,
        sample_rate: u32,
        settings: &CallSettings,
        session: &str,
        events: &mpsc::Sender<Value>,
        dir: &PrivateDir,
    ) -> Recognition {
        let gate_pcm = if sample_rate == 16_000 {
            pcm.clone()
        } else {
            let mut resampler = StreamResampler::new(sample_rate, 16_000, ResamplerQuality::Quick);
            let samples: Vec<f32> = pcm
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair) as f32 / 32768.0)
                .collect();
            let mut output = resampler.process(&samples);
            output.extend(resampler.flush());
            output
                .into_iter()
                .flat_map(|sample| {
                    ((sample * 32767.0).clamp(-32768.0, 32767.0) as i16).to_le_bytes()
                })
                .collect()
        };
        if !crate::pipeline::has_speech(&gate_pcm)
            .await
            .map_err(|_| SttFailure::Provider)?
        {
            return Ok(None);
        }
        let wav = wav(&pcm, sample_rate).ok_or(SttFailure::Provider)?;
        let (text, confidence) = match settings.stt.place.as_str() {
            "device" => {
                let request = Uuid::new_v4().to_string();
                let (tx, rx) = oneshot::channel();
                self.transcripts
                    .lock()
                    .expect("transcripts lock")
                    .insert(request.clone(), tx);
                let language = settings.stt.options.get("language").and_then(Value::as_str);
                let message = json!({"type":"voice-transcribe","data":{
                    "session_id":session,"request_id":request,
                    "audio_base64":base64::engine::general_purpose::STANDARD.encode(wav),
                    "language":language}});
                if events.send(message).await.is_err() {
                    self.transcripts
                        .lock()
                        .expect("transcripts lock")
                        .remove(&request);
                    return Err(SttFailure::Device);
                }
                let result = tokio::time::timeout(device_timeout(), rx).await;
                self.transcripts
                    .lock()
                    .expect("transcripts lock")
                    .remove(&request);
                (
                    match result {
                        Err(_) => return Err(SttFailure::Timeout),
                        Ok(Err(_)) | Ok(Ok(Err(()))) => return Err(SttFailure::Device),
                        Ok(Ok(Ok(text))) => text,
                    },
                    None,
                )
            }
            "openai" => {
                let key = provider_key(dir, "openai").ok_or(SttFailure::Provider)?;
                let client = OpenAiTranscriber::new(&key).map_err(|_| SttFailure::Provider)?;
                let language = settings.stt.options.get("language").and_then(Value::as_str);
                let prompt = settings.stt.options.get("context").and_then(Value::as_str);
                let result = client
                    .transcribe(&wav, &settings.stt.model, language, prompt)
                    .await
                    .map_err(|_| SttFailure::Provider)?;
                (Some(result.text.trim().to_owned()), result.mean_logprob)
            }
            _ => return Err(SttFailure::Provider),
        };
        let Some(text) = text else {
            return Ok(None);
        };
        let text = text.trim().to_owned();
        let language = settings
            .stt
            .options
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or(&settings.ui_language);
        if language != "hi"
            && text.chars().any(char::is_alphabetic)
            && !text.chars().any(|letter| letter.script() == Script::Latin)
        {
            return Ok(None);
        }
        let threshold = if text.split_whitespace().count() <= 2 {
            -3.0
        } else {
            -2.0
        };
        if confidence.is_some_and(|value| value < threshold) {
            return Ok(None);
        }
        Ok((!text.is_empty()).then_some(text))
    }

    pub fn close(&self) {
        self.transcripts.lock().expect("transcripts lock").clear();
    }

    pub async fn listening_bar(&self, playing: bool) {
        if !playing {
            self.playing_uid.lock().expect("playing lock").take();
        }
        self.detector.listening_bar(playing).await;
    }

    pub async fn admitted_receipt(&self, uid: &str, status: &str) {
        let change = {
            let mut playing = self.playing_uid.lock().expect("playing lock");
            if status == "playing" {
                *playing = Some(uid.into());
                Some(true)
            } else if playing.as_deref() == Some(uid)
                && matches!(
                    status,
                    "failed" | "playback_finished" | "cancelled_playing" | "skipped"
                )
            {
                *playing = None;
                Some(false)
            } else {
                None
            }
        };
        if let Some(playing) = change {
            self.detector.listening_bar(playing).await;
        }
    }
}

pub(super) fn provider_key(dir: &PrivateDir, name: &str) -> Option<String> {
    let stored = dir
        .read_json("integrations.json")
        .ok()
        .flatten()
        .and_then(|value| value.get(name).and_then(Value::as_str).map(str::to_owned));
    let environment = match name {
        "openai" => "VOICE_STT_API_KEY",
        "elevenlabs" => "VOICE_ELEVENLABS_API_KEY",
        _ => return None,
    };
    crate::models::effective_key(
        stored.as_deref(),
        std::env::var(environment).ok().as_deref(),
    )
    .map(str::to_owned)
}

fn wav(pcm: &[u8], sample_rate: u32) -> Option<Vec<u8>> {
    if pcm.is_empty() || !pcm.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::new();
    let cursor = std::io::Cursor::new(&mut bytes);
    let mut writer = hound::WavWriter::new(
        cursor,
        hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .ok()?;
    for sample in pcm.as_chunks::<2>().0 {
        writer
            .write_sample(i16::from_le_bytes([sample[0], sample[1]]))
            .ok()?;
    }
    writer.finalize().ok()?;
    Some(bytes)
}

struct PendingTurn {
    turn: VoiceTurn,
    text: String,
    deadline: Instant,
    transcribed_at: u64,
}

struct Catchup {
    pcm: Vec<u8>,
    seq: u64,
    rate: u32,
    truncated: bool,
    time: Option<u64>,
}

enum RecognitionJob {
    Live {
        turn: VoiceTurn,
        pcm: Vec<u8>,
        closed: u64,
    },
    Offline {
        target: VoiceTurn,
        row_id: String,
        pcm: Vec<u8>,
        rate: u32,
        truncated: bool,
        time: Option<u64>,
    },
}

pub(super) struct RecognitionDone {
    job: RecognitionJob,
    result: Recognition,
    transcribed: u64,
}

pub struct TurnOwner {
    media: Arc<CallMedia>,
    room: Arc<Room>,
    settings: CallSettings,
    session: String,
    events: mpsc::Sender<Value>,
    dir: PrivateDir,
    recent: VecDeque<u8>,
    speaking: Option<(VoiceTurn, Vec<u8>)>,
    active: Option<tokio::task::JoinHandle<()>>,
    active_turn: Option<VoiceTurn>,
    queue: VecDeque<RecognitionJob>,
    catchup: Option<Catchup>,
    catchups: u64,
    pending: Option<PendingTurn>,
    pub finished: mpsc::Receiver<RecognitionDone>,
    finished_tx: mpsc::Sender<RecognitionDone>,
}

impl TurnOwner {
    pub fn new(
        media: Arc<CallMedia>,
        room: Arc<Room>,
        settings: CallSettings,
        session: String,
        events: mpsc::Sender<Value>,
        dir: PrivateDir,
    ) -> Self {
        let (finished_tx, finished) = mpsc::channel(16);
        Self {
            media,
            room,
            settings,
            session,
            events,
            dir,
            recent: VecDeque::new(),
            speaking: None,
            active: None,
            active_turn: None,
            queue: VecDeque::new(),
            catchup: None,
            catchups: 0,
            pending: None,
            finished,
            finished_tx,
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending
            .as_ref()
            // A resumed segment keeps the first transcript open through recognition.
            .filter(|_| self.speaking.is_none() && self.active.is_none() && self.queue.is_empty())
            .map(|p| p.deadline)
    }

    pub async fn expired(&mut self) {
        if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            if let Some(pending) = self.pending.take() {
                self.deliver(pending.turn, pending.text, pending.transcribed_at)
                    .await;
            }
        }
    }

    pub async fn frame(&mut self, frame: CallFrame) {
        match frame {
            CallFrame::Audio(bytes) => {
                if let Some((_, pcm)) = &mut self.speaking {
                    if pcm.len() + bytes.len() <= MAX_TURN_BYTES {
                        pcm.extend_from_slice(&bytes);
                    }
                } else {
                    self.recent.extend(bytes);
                    while self.recent.len() > PRE_ROLL_BYTES {
                        self.recent.pop_front();
                    }
                }
            }
            CallFrame::Started => {
                if self.speaking.is_none() {
                    self.media.listening_bar(false).await;
                    if let Ok(turn) = self.room.begin_turn(&self.session) {
                        let _ = self.events.send(json!({"type":"voice-user-turn","data":{
                            "phase":"started","revision":turn.revision,"thread_id":turn.thread_id}})).await;
                        let pcm = self.recent.drain(..).collect();
                        self.speaking = Some((turn, pcm));
                    }
                }
            }
            CallFrame::Stopped { stop_secs } => {
                if let Some((turn, pcm)) = self.speaking.take() {
                    let closed = latency_now_micros();
                    let speech_end = closed.saturating_sub((stop_secs as f64 * 1_000_000.0) as u64);
                    if let Some(thread) = turn.thread_id.as_deref() {
                        self.room.latency_mark(
                            &self.session,
                            thread,
                            turn.revision,
                            None,
                            LatencyEvent::SpeechEnd,
                            speech_end,
                        );
                        self.room.latency_mark(
                            &self.session,
                            thread,
                            turn.revision,
                            None,
                            LatencyEvent::TurnClosed,
                            closed,
                        );
                        self.room.latency_duration(
                            &self.session,
                            thread,
                            turn.revision,
                            None,
                            "endpoint_silence_ms",
                            stop_secs as f64 * 1000.0,
                        );
                        self.room.latency_duration(
                            &self.session,
                            thread,
                            turn.revision,
                            None,
                            "audio_ms",
                            pcm.len() as f64 / 32.0,
                        );
                    }
                    self.enqueue(RecognitionJob::Live { turn, pcm, closed })
                        .await;
                }
            }
        }
    }

    pub async fn result(&mut self, done: RecognitionDone) {
        if let RecognitionJob::Live { turn, .. } = &done.job {
            if self.active_turn.as_ref().map(|active| active.revision) != Some(turn.revision) {
                return;
            }
        }
        if let Some(active) = self.active.take() {
            let _ = active.await;
        }
        self.active_turn = None;
        let RecognitionDone {
            job,
            result,
            transcribed,
        } = done;
        match job {
            RecognitionJob::Live { turn, closed, .. } => {
                self.live_result(turn, result, closed, transcribed).await;
            }
            RecognitionJob::Offline {
                target,
                row_id,
                truncated,
                time,
                ..
            } => match result {
                Ok(Some(text)) => {
                    let offline = if truncated { "truncated" } else { "buffered" };
                    let _ = self.events.send(json!({"type":"voice-catchup-turn","data":{
                            "session_id":self.session,"history_id":row_id,"thread_id":target.thread_id,
                            "text":text,"offline":offline,"time":time}})).await;
                    let _ = self
                        .room
                        .queue_offline_input(&target, &row_id, &text, offline, time);
                }
                Err(error) => self.report_error(error, true).await,
                Ok(None) => {}
            },
        }
        self.start_next();
    }

    async fn live_result(
        &mut self,
        turn: VoiceTurn,
        result: Recognition,
        closed: u64,
        transcribed: u64,
    ) {
        if self.room.turn_cancelled(&self.session, turn.revision) {
            self.room.finish_turn(&self.session, turn.revision);
            return;
        }
        if let Some(thread) = turn.thread_id.as_deref() {
            self.room.latency_mark(
                &self.session,
                thread,
                turn.revision,
                None,
                LatencyEvent::Transcript,
                transcribed,
            );
            self.room.latency_duration(
                &self.session,
                thread,
                turn.revision,
                None,
                "recognition_ms",
                transcribed.saturating_sub(closed) as f64 / 1000.0,
            );
            self.room.latency_duration(
                &self.session,
                thread,
                turn.revision,
                None,
                "request_to_transcript_ms",
                transcribed.saturating_sub(closed) as f64 / 1000.0,
            );
        }
        let text = match result {
            Ok(text) => text.unwrap_or_default(),
            Err(error) => {
                self.report_error(error, false).await;
                String::new()
            }
        };
        let current = self.room.snapshot(Some(&self.session));
        let revision = current["room"]["revision"].as_u64().unwrap_or(0);
        let same_focus = current["binding"]["thread_id"].as_str() == turn.thread_id.as_deref();
        if text.is_empty() {
            let _ = self.events.send(json!({"type":"voice-user-turn","data":{
                "phase":"cancelled","revision":turn.revision,"thread_id":turn.thread_id,"text":""}})).await;
            self.room.finish_turn(&self.session, turn.revision);
            return;
        }
        if revision > turn.revision && same_focus {
            self.hold(turn, text, transcribed).await;
            return;
        }
        if revision > turn.revision || self.settings.merge_window_secs <= 0.0 {
            self.deliver(turn, text, transcribed).await;
            return;
        }
        if let Some(previous) = self.pending.take() {
            if previous.turn.thread_id == turn.thread_id {
                self.room.finish_turn(&self.session, previous.turn.revision);
                self.pending = Some(PendingTurn {
                    turn,
                    text: format!("{} {}", previous.text, text),
                    deadline: Instant::now()
                        + Duration::from_secs_f32(self.settings.merge_window_secs),
                    transcribed_at: transcribed,
                });
                return;
            }
            self.deliver(previous.turn, previous.text, previous.transcribed_at)
                .await;
        }
        self.pending = Some(PendingTurn {
            turn,
            text,
            deadline: Instant::now() + Duration::from_secs_f32(self.settings.merge_window_secs),
            transcribed_at: transcribed,
        });
    }

    async fn hold(&mut self, turn: VoiceTurn, text: String, transcribed: u64) {
        if let Some(previous) = self.pending.take() {
            if previous.turn.thread_id == turn.thread_id {
                self.room.finish_turn(&self.session, previous.turn.revision);
                self.pending = Some(PendingTurn {
                    turn,
                    text: format!("{} {}", previous.text, text),
                    deadline: Instant::now()
                        + Duration::from_secs_f32(self.settings.merge_window_secs),
                    transcribed_at: transcribed,
                });
                return;
            }
            self.deliver(previous.turn, previous.text, previous.transcribed_at)
                .await;
        }
        let _ = self.events.send(json!({"type":"voice-user-turn","data":{
            "phase":"cancelled","revision":turn.revision,"thread_id":turn.thread_id,"text":text,"merged":true}})).await;
        self.pending = Some(PendingTurn {
            turn,
            text,
            deadline: Instant::now(),
            transcribed_at: transcribed,
        });
    }

    async fn deliver(&mut self, turn: VoiceTurn, text: String, transcribed_at: u64) {
        if self.room.turn_cancelled(&self.session, turn.revision) {
            self.room.finish_turn(&self.session, turn.revision);
            return;
        }
        let _ = self
            .events
            .send(json!({"type":"voice-user-turn","data":{
            "phase":"finished","revision":turn.revision,"thread_id":turn.thread_id,"text":text}}))
            .await;
        let _ = self.room.queue_voice_input(&turn, &text);
        if let Some(thread) = turn.thread_id.as_deref() {
            let delivered = latency_now_micros();
            self.room.latency_mark(
                &self.session,
                thread,
                turn.revision,
                None,
                LatencyEvent::TranscriptDelivered,
                delivered,
            );
            self.room.latency_duration(
                &self.session,
                thread,
                turn.revision,
                None,
                "transcript_to_delivery_ms",
                delivered.saturating_sub(transcribed_at) as f64 / 1000.0,
            );
        }
        self.room.finish_turn(&self.session, turn.revision);
    }

    async fn report_error(&self, error: SttFailure, offline: bool) {
        use crate::messages::{render, LocalizedMessage};
        let key = match (offline, error) {
            (true, _) => "voice.catchup_transcription_failed",
            (false, SttFailure::Timeout) => "voice.transcription_timeout",
            (false, _) => "voice.transcription_failed",
        };
        let message = render(&LocalizedMessage::new(key), &self.settings.ui_language);
        let _ = self
            .events
            .send(json!({"type":"error","data":{"message":message}}))
            .await;
    }

    async fn enqueue(&mut self, job: RecognitionJob) {
        if self.queue.len() + usize::from(self.active.is_some()) >= MAX_RECOGNITION_QUEUE {
            if let RecognitionJob::Live { turn, .. } = job {
                let _ = self.events.send(json!({"type":"voice-user-turn","data":{
                    "phase":"cancelled","revision":turn.revision,"thread_id":turn.thread_id,"text":""}})).await;
                self.room.finish_turn(&self.session, turn.revision);
            }
            self.report_error(SttFailure::Provider, false).await;
            return;
        }
        self.queue.push_back(job);
        self.start_next();
    }

    fn start_next(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(mut job) = self.queue.pop_front() else {
            return;
        };
        self.active_turn = match &job {
            RecognitionJob::Live { turn, .. } => Some(turn.clone()),
            RecognitionJob::Offline { .. } => None,
        };
        let media = self.media.clone();
        let settings = self.settings.clone();
        let session = self.session.clone();
        let events = self.events.clone();
        let dir = self.dir.clone();
        let finished = self.finished_tx.clone();
        self.active = Some(tokio::spawn(async move {
            let (pcm, rate) = match &mut job {
                RecognitionJob::Live { pcm, .. } => (std::mem::take(pcm), 16_000),
                RecognitionJob::Offline { pcm, rate, .. } => (std::mem::take(pcm), *rate),
            };
            let result = media
                .recognize(pcm, rate, &settings, &session, &events, &dir)
                .await;
            let _ = finished
                .send(RecognitionDone {
                    job,
                    result,
                    transcribed: latency_now_micros(),
                })
                .await;
        }));
    }

    pub async fn catchup_slice(&mut self, data: &Value) {
        let rate = data
            .get("sample_rate")
            .and_then(Value::as_u64)
            .filter(|rate| (8_000..=48_000).contains(rate))
            .map(|rate| rate as u32);
        let seq = data.get("seq").and_then(Value::as_u64);
        let (Some(rate), Some(seq)) = (rate, seq) else {
            self.catchup = None;
            return;
        };
        if seq == 0 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let time = data
                .get("started_at")
                .and_then(Value::as_f64)
                .filter(|at| {
                    at.is_finite()
                        && *at >= now.saturating_sub(3_600_000) as f64
                        && *at <= (now + 60_000) as f64
                })
                .map(|at| at as u64);
            self.catchup = Some(Catchup {
                pcm: Vec::new(),
                seq: 0,
                rate,
                truncated: data["truncated"].as_bool().unwrap_or(false),
                time,
            });
        }
        let Some(pending) = &mut self.catchup else {
            return;
        };
        if pending.seq != seq || pending.rate != rate {
            self.catchup = None;
            return;
        }
        let Some(encoded) = data.get("audio_base64").and_then(Value::as_str) else {
            self.catchup = None;
            return;
        };
        let Ok(audio) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
            self.catchup = None;
            return;
        };
        if audio.len() > CATCHUP_SLICE_BYTES
            || pending.pcm.len() + audio.len() > CATCHUP_MAX_SECONDS * rate as usize * 2
        {
            self.catchup = None;
            use crate::messages::{render, LocalizedMessage};
            let message = render(
                &LocalizedMessage::new("voice.catchup_too_long"),
                &self.settings.ui_language,
            );
            let _ = self
                .events
                .send(json!({"type":"error","data":{"message":message}}))
                .await;
            return;
        }
        pending.pcm.extend_from_slice(&audio);
        pending.seq += 1;
        if data["final"].as_bool() != Some(true) {
            return;
        }
        let pending = self.catchup.take().expect("catchup exists");
        let Some(target) = self.room.offline_target(&self.session) else {
            return;
        };
        self.catchups += 1;
        let row_id = format!("{}:user-catchup:{}", self.session, self.catchups);
        self.enqueue(RecognitionJob::Offline {
            target,
            row_id,
            pcm: pending.pcm,
            rate,
            truncated: pending.truncated,
            time: pending.time,
        })
        .await;
    }

    pub async fn close(&mut self) {
        self.media.close();
        if let Some(active) = self.active.take() {
            active.abort();
            let _ = active.await;
        }
        if let Some(turn) = self.active_turn.take() {
            self.room.finish_turn(&self.session, turn.revision);
        }
        for job in self.queue.drain(..) {
            if let RecognitionJob::Live { turn, .. } = job {
                self.room.finish_turn(&self.session, turn.revision);
            }
        }
        self.catchup = None;
        if let Some((turn, _)) = self.speaking.take() {
            self.room.finish_turn(&self.session, turn.revision);
        }
        if let Some(pending) = self.pending.take() {
            self.room.finish_turn(&self.session, pending.turn.revision);
        }
    }

    pub async fn cancel(&mut self, revision: u64) {
        if self
            .speaking
            .as_ref()
            .is_some_and(|(turn, _)| turn.revision == revision)
        {
            self.speaking = None;
            self.recent.clear();
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.turn.revision == revision)
        {
            self.pending = None;
        }
        self.queue.retain(|job| {
            !matches!(job, RecognitionJob::Live { turn, .. } if turn.revision == revision)
        });
        if self.active_turn.as_ref().map(|turn| turn.revision) == Some(revision) {
            if let Some(active) = self.active.take() {
                active.abort();
                let _ = active.await;
            }
            self.active_turn = None;
            self.start_next();
        }
        self.room.finish_turn(&self.session, revision);
    }

    pub async fn focus_changed(&mut self) {
        if let Some((turn, pcm)) = self.speaking.take() {
            let closed = latency_now_micros();
            if let Some(thread) = turn.thread_id.as_deref() {
                self.room.latency_mark(
                    &self.session,
                    thread,
                    turn.revision,
                    None,
                    LatencyEvent::TurnClosed,
                    closed,
                );
            }
            self.enqueue(RecognitionJob::Live { turn, pcm, closed })
                .await;
            if let Ok(turn) = self.room.begin_turn(&self.session) {
                let _ = self
                    .events
                    .send(json!({"type":"voice-user-turn","data":{
                    "phase":"started","revision":turn.revision,"thread_id":turn.thread_id}}))
                    .await;
                self.speaking = Some((turn, Vec::new()));
            }
        }
    }
}

pub async fn speech_event(
    room: Arc<Room>,
    session: &str,
    settings: &CallSettings,
    dir: &PrivateDir,
    cache: Arc<SynthesisCache>,
    event: Value,
    replay_audio: Option<Arc<crate::providers::CloudSpeech>>,
) -> Option<Value> {
    if event.get("type").and_then(Value::as_str) != Some("voice-speech") {
        return Some(event);
    }
    let data = &event["data"];
    let uid = data["utterance_id"].as_str()?;
    let revision = data["revision"].as_u64()?;
    let reply_revision = data["reply_revision"].as_u64().unwrap_or(revision);
    let thread = data["thread_id"].as_str()?;
    let text = data["text"].as_str()?;
    let voice = resolve_voice(settings, data["language"].as_str()).ok()?;
    let mut message = data.clone();
    let object = message.as_object_mut()?;
    object.insert("place".into(), json!(&voice.place));
    object.insert("model".into(), json!(&voice.model));
    object.insert("voice".into(), json!(&voice.voice));
    object.insert("speed".into(), json!(voice.speed));
    object.insert("language".into(), json!(&voice.language));
    if voice.place == "device" {
        if !room.speech_current(session, uid, revision) {
            return None;
        }
        room.latency_mark(
            session,
            thread,
            reply_revision,
            Some(uid),
            LatencyEvent::AudioDispatched,
            latency_now_micros(),
        );
        return Some(json!({"type":"voice-speech","data":message}));
    }
    let choice = SynthesisChoice {
        place: &voice.place,
        model: &voice.model,
        voice: &voice.voice,
        speed: voice.speed,
    };
    let result = if let Some(speech) = replay_audio {
        crate::providers::cache::CachedSpeech { speech, fresh: false }
    } else {
        let key = provider_key(dir, "elevenlabs")?;
        let client = ElevenLabsTts::new(&key).ok()?;
        let model = voice.model.clone();
        let voice_id = voice.voice.clone();
        let text_owned = text.to_owned();
        cache
            .obtain(choice, text, move || async move {
                client
                    .synthesize(
                        &text_owned,
                        &model,
                        &voice_id,
                        voice.speed,
                        true,
                        "mp3_44100_128",
                    )
                    .await
            })
            .await
            .ok()?
    };
    room.latency_mark(
        session,
        thread,
        reply_revision,
        Some(uid),
        LatencyEvent::AudioReady,
        latency_now_micros(),
    );
    if !room.speech_current(session, uid, revision) {
        return None;
    }
    object.insert("mime_type".into(), json!(&result.speech.mime_type));
    object.insert(
        "audio_base64".into(),
        json!(base64::engine::general_purpose::STANDARD.encode(&result.speech.audio)),
    );
    object.insert("alignment".into(), json!(result.speech.alignment));
    object.insert(
        "timings_ms".into(),
        json!(if result.fresh {
            result.speech.timings_ms.clone()
        } else {
            serde_json::Map::new()
        }),
    );
    object.insert("shared".into(), json!(!result.fresh));
    if result.fresh {
        for name in [
            "request_to_headers_ms",
            "request_to_first_chunk_ms",
            "request_to_complete_ms",
        ] {
            if let Some(value) = result.speech.timings_ms.get(name).and_then(Value::as_f64) {
                room.latency_duration(session, thread, reply_revision, Some(uid), name, value);
            }
        }
    }
    room.latency_mark(
        session,
        thread,
        reply_revision,
        Some(uid),
        LatencyEvent::AudioDispatched,
        latency_now_micros(),
    );
    Some(json!({"type":"voice-speech-audio","data":message}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recognition_queue_is_bounded_and_drained_on_close() {
        let directory = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open(directory.path().join("private")).unwrap();
        let room = Arc::new(Room::load(dir.clone()).unwrap());
        let (events, _received) = mpsc::channel(64);
        let sid = room
            .join("device".into(), "en".into(), events.clone())
            .unwrap();
        let mut settings = crate::models::default_settings(None, None);
        settings.turn_end_mode = "timer".into();
        let (media, _frames, _focus) = CallMedia::start(&settings).unwrap();
        let mut owner = TurnOwner::new(media, room.clone(), settings, sid.clone(), events, dir);
        for _ in 0..MAX_RECOGNITION_QUEUE + 3 {
            owner.frame(CallFrame::Audio(vec![0; 2048])).await;
            owner.frame(CallFrame::Started).await;
            owner.frame(CallFrame::Stopped { stop_secs: 0.5 }).await;
        }
        assert!(owner.active.is_some());
        assert_eq!(owner.queue.len(), MAX_RECOGNITION_QUEUE - 1);
        owner.close().await;
        assert!(owner.active.is_none() && owner.queue.is_empty());
        assert!(room.history(None)["messages"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
