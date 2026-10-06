//! One recognition: the speech gate, the configured transcriber, then the acceptance filters.

use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use unicode_script::{Script, UnicodeScript};
use uuid::Uuid;

use super::{
    audio::{to_gate_rate, wav},
    keys::provider_key,
    transcripts::DeviceTranscripts,
};
use crate::{providers::OpenAiTranscriber, storage::PrivateDir, types::CallSettings};

#[derive(Debug)]
pub(super) enum SttFailure {
    Timeout,
    Device,
    Provider,
}

pub(super) type Recognition = Result<Option<String>, SttFailure>;

/// Transcript text, if any, and the transcriber's mean log-probability when it reports one.
type Transcribed = (Option<String>, Option<f64>);

/// Everything one recognition reads; borrowed from the call for its duration.
pub(super) struct Recognizer<'a> {
    pub(super) transcripts: &'a DeviceTranscripts,
    pub(super) settings: &'a CallSettings,
    pub(super) session: &'a str,
    pub(super) events: &'a mpsc::Sender<Value>,
    pub(super) dir: &'a PrivateDir,
}

impl Recognizer<'_> {
    pub(super) async fn recognize(&self, pcm: Vec<u8>, sample_rate: u32) -> Recognition {
        let gate_pcm = to_gate_rate(&pcm, sample_rate);
        if !crate::pipeline::has_speech(&gate_pcm)
            .await
            .map_err(|_| SttFailure::Provider)?
        {
            return Ok(None);
        }
        let wav = wav(&pcm, sample_rate).ok_or(SttFailure::Provider)?;
        let (text, confidence) = match self.settings.stt.place.as_str() {
            "device" => (self.on_device(wav).await?, None),
            "openai" => self.with_openai(&wav).await?,
            _ => return Err(SttFailure::Provider),
        };
        Ok(text.and_then(|text| accepted(text.trim(), confidence, self.settings)))
    }

    fn language(&self) -> Option<&str> {
        recognition_language(self.settings)
    }

    /// Asks the browser to transcribe and waits for its reply on the call socket.
    async fn on_device(&self, wav: Vec<u8>) -> Result<Option<String>, SttFailure> {
        let request = Uuid::new_v4().to_string();
        let reply = self.transcripts.open(&request);
        let message = json!({"type":"voice-transcribe","data":{
            "session_id":self.session,"request_id":request,
            "audio_base64":base64::engine::general_purpose::STANDARD.encode(wav),
            "language":self.language()}});
        if self.events.send(message).await.is_err() {
            self.transcripts.forget(&request);
            return Err(SttFailure::Device);
        }
        let result = tokio::time::timeout(device_timeout(), reply).await;
        self.transcripts.forget(&request);
        match result {
            Err(_) => Err(SttFailure::Timeout),
            Ok(Err(_)) | Ok(Ok(Err(()))) => Err(SttFailure::Device),
            Ok(Ok(Ok(text))) => Ok(text),
        }
    }

    async fn with_openai(&self, wav: &[u8]) -> Result<Transcribed, SttFailure> {
        let key = provider_key(self.dir, "openai").ok_or(SttFailure::Provider)?;
        let client = OpenAiTranscriber::new(&key).map_err(|_| SttFailure::Provider)?;
        let prompt = self
            .settings
            .stt
            .options
            .get("context")
            .and_then(Value::as_str);
        let result = client
            .transcribe(wav, &self.settings.stt.model, self.language(), prompt)
            .await
            .map_err(|_| SttFailure::Provider)?;
        Ok((Some(result.text.trim().to_owned()), result.mean_logprob))
    }
}

/// Drops transcripts that are empty, in an unexpected script, or too unlikely.
pub(super) fn accepted(
    text: &str,
    confidence: Option<f64>,
    settings: &CallSettings,
) -> Option<String> {
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
        return None;
    }
    let threshold = if text.split_whitespace().count() <= 2 {
        -3.0
    } else {
        -2.0
    };
    if confidence.is_some_and(|value| value < threshold) {
        return None;
    }
    (!text.is_empty()).then(|| text.to_owned())
}

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

/// The language a recogniser is asked for: none when the stage detects it (`auto`).
pub(super) fn recognition_language(settings: &CallSettings) -> Option<&str> {
    settings
        .stt
        .options
        .get("language")
        .and_then(Value::as_str)
        .filter(|language| *language != "auto")
}
