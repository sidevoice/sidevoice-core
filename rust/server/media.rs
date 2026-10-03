//! One microphone source and one turn lifecycle for an authenticated call.

use std::{collections::{HashMap, VecDeque}, sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex}, time::{Duration, Instant}};

use base64::Engine;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{
    control::room::{latency_now_micros, LatencyEvent, Room, VoiceTurn},
    models::resolve_voice,
    pipeline::{CallDetector, CallFrame},
    providers::{cache::{SynthesisCache, SynthesisChoice}, ElevenLabsTts, OpenAiTranscriber},
    storage::PrivateDir,
    types::CallSettings,
};

const MAX_TURN_BYTES: usize = 16_000 * 2 * 60;
const PRE_ROLL_BYTES: usize = 16_000 * 2;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source { Socket, WebRtc }

pub struct CallMedia {
    detector: CallDetector,
    source: Mutex<Source>,
    transcripts: Mutex<HashMap<String, oneshot::Sender<Option<String>>>>,
    rtc_generation: AtomicU64,
    rtc: Mutex<Option<Arc<dyn webrtc::peer_connection::PeerConnection>>>,
}

impl CallMedia {
    pub fn start(settings: &CallSettings) -> Result<(Arc<Self>, mpsc::Receiver<CallFrame>), String> {
        let (detector, events) = CallDetector::start(settings)?;
        Ok((Arc::new(Self {
            detector,
            source: Mutex::new(Source::Socket),
            transcripts: Mutex::new(HashMap::new()),
            rtc_generation: AtomicU64::new(0),
            rtc: Mutex::new(None),
        }), events))
    }

    pub fn select(&self, source: Source) { *self.source.lock().expect("source lock") = source; }

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
        if let Some(prior) = prior { let _ = prior.close().await; }
        generation
    }

    pub async fn set_rtc(&self, generation: u64, peer: Arc<dyn webrtc::peer_connection::PeerConnection>) {
        if self.rtc_generation.load(Ordering::Acquire) == generation {
            self.rtc.lock().expect("rtc lock").replace(peer);
        } else { let _ = peer.close().await; }
    }

    pub async fn close_rtc(&self) {
        self.rtc_generation.fetch_add(1, Ordering::AcqRel);
        let peer = self.rtc.lock().expect("rtc lock").take();
        if let Some(peer) = peer { let _ = peer.close().await; }
    }

    pub fn transcript(&self, data: &Value, error: bool, session: &str) {
        if data.get("session_id").and_then(Value::as_str) != Some(session) { return; }
        let Some(request) = data.get("request_id").and_then(Value::as_str) else { return; };
        let pending = self.transcripts.lock().expect("transcripts lock").remove(request);
        if let Some(pending) = pending {
            let text = if error { None } else { data.get("text").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned) };
            let _ = pending.send(text);
        }
    }

    async fn recognize(&self, pcm: Vec<u8>, settings: &CallSettings, session: &str,
                       events: &mpsc::Sender<Value>, dir: &PrivateDir) -> Option<String> {
        let wav = wav(&pcm)?;
        match settings.stt.place.as_str() {
            "device" => {
                let request = Uuid::new_v4().to_string();
                let (tx, rx) = oneshot::channel();
                self.transcripts.lock().expect("transcripts lock").insert(request.clone(), tx);
                let language = settings.stt.options.get("language").and_then(Value::as_str);
                let message = json!({"type":"voice-transcribe","data":{
                    "session_id":session,"request_id":request,
                    "audio_base64":base64::engine::general_purpose::STANDARD.encode(wav),
                    "language":language}});
                if events.send(message).await.is_err() { self.transcripts.lock().expect("transcripts lock").remove(&request); return None; }
                let result = tokio::time::timeout(Duration::from_secs(90), rx).await.ok().and_then(Result::ok).flatten();
                self.transcripts.lock().expect("transcripts lock").remove(&request);
                result
            }
            "openai" => {
                let key = provider_key(dir, "openai")?;
                let client = OpenAiTranscriber::new(&key).ok()?;
                let language = settings.stt.options.get("language").and_then(Value::as_str);
                let prompt = settings.stt.options.get("context").and_then(Value::as_str);
                client.transcribe(&wav, &settings.stt.model, language, prompt).await.ok().map(|result| result.text.trim().to_owned())
            }
            _ => None,
        }
    }

    pub fn close(&self) { self.transcripts.lock().expect("transcripts lock").clear(); }
}

pub(super) fn provider_key(dir: &PrivateDir, name: &str) -> Option<String> {
    let stored = dir.read_json("integrations.json").ok().flatten()
        .and_then(|value| value.get(name).and_then(Value::as_str).map(str::to_owned));
    let environment = match name { "openai" => "VOICE_STT_API_KEY", "elevenlabs" => "VOICE_ELEVENLABS_API_KEY", _ => return None };
    crate::models::effective_key(stored.as_deref(), std::env::var(environment).ok().as_deref()).map(str::to_owned)
}

fn wav(pcm: &[u8]) -> Option<Vec<u8>> {
    if pcm.is_empty() || pcm.len() % 2 != 0 { return None; }
    let mut bytes = Vec::new();
    let cursor = std::io::Cursor::new(&mut bytes);
    let mut writer = hound::WavWriter::new(cursor, hound::WavSpec {
        channels: 1, sample_rate: 16_000, bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    }).ok()?;
    for sample in pcm.chunks_exact(2) {
        writer.write_sample(i16::from_le_bytes([sample[0], sample[1]])).ok()?;
    }
    writer.finalize().ok()?;
    Some(bytes)
}

struct PendingTurn { turn: VoiceTurn, text: String, deadline: Instant, transcribed_at: u64 }

pub struct TurnOwner {
    media: Arc<CallMedia>,
    room: Arc<Room>,
    settings: CallSettings,
    session: String,
    events: mpsc::Sender<Value>,
    dir: PrivateDir,
    recent: VecDeque<u8>,
    speaking: Option<(VoiceTurn, Vec<u8>)>,
    pending: Option<PendingTurn>,
    pub finished: mpsc::Receiver<(VoiceTurn, Option<String>, u64, u64, usize)>,
    finished_tx: mpsc::Sender<(VoiceTurn, Option<String>, u64, u64, usize)>,
}

impl TurnOwner {
    pub fn new(media: Arc<CallMedia>, room: Arc<Room>, settings: CallSettings, session: String,
               events: mpsc::Sender<Value>, dir: PrivateDir) -> Self {
        let (finished_tx, finished) = mpsc::channel(16);
        Self { media, room, settings, session, events, dir, recent: VecDeque::new(),
            speaking: None, pending: None, finished, finished_tx }
    }

    pub fn deadline(&self) -> Option<Instant> { self.pending.as_ref().filter(|_| self.speaking.is_none()).map(|p| p.deadline) }

    pub async fn expired(&mut self) {
        if self.deadline().is_some_and(|deadline| Instant::now() >= deadline) {
            if let Some(pending) = self.pending.take() { self.deliver(pending.turn, pending.text, pending.transcribed_at).await; }
        }
    }

    pub async fn frame(&mut self, frame: CallFrame) {
        match frame {
            CallFrame::Audio(bytes) => {
                if let Some((_, pcm)) = &mut self.speaking {
                    if pcm.len() + bytes.len() <= MAX_TURN_BYTES { pcm.extend_from_slice(&bytes); }
                } else {
                    self.recent.extend(bytes);
                    while self.recent.len() > PRE_ROLL_BYTES { self.recent.pop_front(); }
                }
            }
            CallFrame::Started => {
                if self.speaking.is_none() {
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
                        self.room.latency_mark(&self.session,thread,turn.revision,None,LatencyEvent::SpeechEnd,speech_end);
                        self.room.latency_mark(&self.session,thread,turn.revision,None,LatencyEvent::TurnClosed,closed);
                        self.room.latency_duration(&self.session,thread,turn.revision,None,"endpoint_silence_ms",stop_secs as f64 * 1000.0);
                        self.room.latency_duration(&self.session,thread,turn.revision,None,"audio_ms",pcm.len() as f64 / 32.0);
                    }
                    let bytes = pcm.len();
                    let media = self.media.clone();
                    let settings = self.settings.clone();
                    let session = self.session.clone();
                    let events = self.events.clone();
                    let dir = self.dir.clone();
                    let finished = self.finished_tx.clone();
                    tokio::spawn(async move {
                        let text = media.recognize(pcm, &settings, &session, &events, &dir).await;
                        let _ = finished.send((turn, text, closed, latency_now_micros(), bytes)).await;
                    });
                }
            }
        }
    }

    pub async fn next_result(&mut self) { if let Some((turn, text, closed, transcribed, bytes)) = self.finished.recv().await { self.result(turn, text, closed, transcribed, bytes).await; } }

    pub async fn result(&mut self, turn: VoiceTurn, text: Option<String>, closed: u64, transcribed: u64, _bytes: usize) {
        if let Some(thread) = turn.thread_id.as_deref() {
            self.room.latency_mark(&self.session,thread,turn.revision,None,LatencyEvent::Transcript,transcribed);
            self.room.latency_duration(&self.session,thread,turn.revision,None,"recognition_ms",transcribed.saturating_sub(closed) as f64 / 1000.0);
            self.room.latency_duration(&self.session,thread,turn.revision,None,"request_to_transcript_ms",transcribed.saturating_sub(closed) as f64 / 1000.0);
        }
        let text = text.unwrap_or_default();
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
                self.pending = Some(PendingTurn { turn, text: format!("{} {}", previous.text, text),
                    deadline: Instant::now() + Duration::from_secs_f32(self.settings.merge_window_secs), transcribed_at: transcribed });
                return;
            }
            self.deliver(previous.turn, previous.text, previous.transcribed_at).await;
        }
        self.pending = Some(PendingTurn { turn, text,
            deadline: Instant::now() + Duration::from_secs_f32(self.settings.merge_window_secs), transcribed_at: transcribed });
    }

    async fn hold(&mut self, turn: VoiceTurn, text: String, transcribed: u64) {
        if let Some(previous) = self.pending.take() {
            if previous.turn.thread_id == turn.thread_id {
                self.room.finish_turn(&self.session, previous.turn.revision);
                self.pending = Some(PendingTurn { turn, text: format!("{} {}", previous.text, text),
                    deadline: Instant::now() + Duration::from_secs_f32(self.settings.merge_window_secs), transcribed_at: transcribed });
                return;
            }
            self.deliver(previous.turn, previous.text, previous.transcribed_at).await;
        }
        let _ = self.events.send(json!({"type":"voice-user-turn","data":{
            "phase":"cancelled","revision":turn.revision,"thread_id":turn.thread_id,"text":text,"merged":true}})).await;
        self.pending = Some(PendingTurn { turn, text, deadline: Instant::now(), transcribed_at: transcribed });
    }

    async fn deliver(&mut self, turn: VoiceTurn, text: String, transcribed_at: u64) {
        let _ = self.events.send(json!({"type":"voice-user-turn","data":{
            "phase":"finished","revision":turn.revision,"thread_id":turn.thread_id,"text":text}})).await;
        let _ = self.room.queue_voice_input(&turn, &text);
        if let Some(thread) = turn.thread_id.as_deref() {
            let delivered = latency_now_micros();
            self.room.latency_mark(&self.session,thread,turn.revision,None,LatencyEvent::TranscriptDelivered,delivered);
            self.room.latency_duration(&self.session,thread,turn.revision,None,"transcript_to_delivery_ms",delivered.saturating_sub(transcribed_at) as f64 / 1000.0);
        }
        self.room.finish_turn(&self.session, turn.revision);
    }

    pub async fn close(&mut self) {
        if let Some((turn, _)) = self.speaking.take() { self.room.finish_turn(&self.session, turn.revision); }
        if let Some(pending) = self.pending.take() { self.deliver(pending.turn, pending.text, pending.transcribed_at).await; }
    }
}

pub async fn speech_event(room: Arc<Room>, session: &str, settings: &CallSettings,
                          dir: &PrivateDir, cache: Arc<SynthesisCache>, event: Value) -> Option<Value> {
    if event.get("type").and_then(Value::as_str) != Some("voice-speech") { return Some(event); }
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
        room.latency_mark(session,thread,reply_revision,Some(uid),LatencyEvent::AudioDispatched,latency_now_micros());
        return Some(json!({"type":"voice-speech","data":message}));
    }
    let key = provider_key(dir, "elevenlabs")?;
    let client = ElevenLabsTts::new(&key).ok()?;
    let choice = SynthesisChoice { place: &voice.place, model: &voice.model, voice: &voice.voice, speed: voice.speed };
    let model = voice.model.clone();
    let voice_id = voice.voice.clone();
    let text_owned = text.to_owned();
    let result = cache.obtain(choice, text, move || async move {
        client.synthesize(&text_owned, &model, &voice_id, voice.speed, true, "mp3_44100_128").await
    }).await.ok()?;
    room.latency_mark(session,thread,reply_revision,Some(uid),LatencyEvent::AudioReady,latency_now_micros());
    let snapshot = room.snapshot(Some(session));
    if snapshot["room"]["revision"].as_u64() != Some(revision)
        || snapshot["room"]["speaking"].as_bool() == Some(true)
        || !snapshot["call"]["utterances"].as_array().is_some_and(|rows| rows.iter().any(|row| row["utterance_id"] == uid && !matches!(row["status"].as_str(), Some("interrupted" | "failed" | "playback_finished")))) {
        return None;
    }
    object.insert("mime_type".into(), json!(&result.speech.mime_type));
    object.insert("audio_base64".into(), json!(base64::engine::general_purpose::STANDARD.encode(&result.speech.audio)));
    object.insert("alignment".into(), json!(result.speech.alignment));
    object.insert("timings_ms".into(), json!(if result.fresh { result.speech.timings_ms.clone() } else { serde_json::Map::new() }));
    object.insert("shared".into(), json!(!result.fresh));
    if result.fresh {
        for name in ["request_to_headers_ms","request_to_first_chunk_ms","request_to_complete_ms"] {
            if let Some(value) = result.speech.timings_ms.get(name).and_then(Value::as_f64) {
                room.latency_duration(session,thread,reply_revision,Some(uid),name,value);
            }
        }
    }
    room.latency_mark(session,thread,reply_revision,Some(uid),LatencyEvent::AudioDispatched,latency_now_micros());
    Some(json!({"type":"voice-speech-audio","data":message}))
}
