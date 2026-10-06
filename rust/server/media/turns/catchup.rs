//! Audio the browser buffered offline arrives as ordered slices; the final one
//! queues it for recognition into the room's catch-up input.

use base64::Engine;
use serde_json::{json, Value};

use crate::control::room::VoiceTurn;

use super::super::recognition::Recognition;
use super::{queue::RecognitionJob, TurnOwner};

const CATCHUP_SLICE_BYTES: usize = 128 * 1024;
const CATCHUP_MAX_SECONDS: usize = 35;

pub(super) struct Catchup {
    pcm: Vec<u8>,
    seq: u64,
    rate: u32,
    truncated: bool,
    time: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SliceError {
    /// Out of order, a changed rate, or audio that is missing or not base64.
    Invalid,
    /// A slice or the whole recording is over its limit.
    TooLong,
}

impl Catchup {
    pub(super) fn begin(data: &Value, rate: u32, now_millis: u64) -> Self {
        Self {
            pcm: Vec::new(),
            seq: 0,
            rate,
            truncated: data["truncated"].as_bool().unwrap_or(false),
            time: started_at(data, now_millis),
        }
    }

    /// Appends the next slice; `Ok(true)` when it was the final one.
    pub(super) fn append(&mut self, data: &Value, rate: u32, seq: u64) -> Result<bool, SliceError> {
        if self.seq != seq || self.rate != rate {
            return Err(SliceError::Invalid);
        }
        let encoded = data
            .get("audio_base64")
            .and_then(Value::as_str)
            .ok_or(SliceError::Invalid)?;
        let audio = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| SliceError::Invalid)?;
        if audio.len() > CATCHUP_SLICE_BYTES
            || self.pcm.len() + audio.len() > CATCHUP_MAX_SECONDS * rate as usize * 2
        {
            return Err(SliceError::TooLong);
        }
        self.pcm.extend_from_slice(&audio);
        self.seq += 1;
        Ok(data["final"].as_bool() == Some(true))
    }
}

/// The slice's sample rate and sequence number, if both are present and valid.
pub(super) fn slice_header(data: &Value) -> Option<(u32, u64)> {
    let rate = data
        .get("sample_rate")
        .and_then(Value::as_u64)
        .filter(|rate| (8_000..=48_000).contains(rate))?;
    let seq = data.get("seq").and_then(Value::as_u64)?;
    Some((rate as u32, seq))
}

/// When the recording started, trusted only within the last hour or a minute ahead.
pub(super) fn started_at(data: &Value, now_millis: u64) -> Option<u64> {
    data.get("started_at")
        .and_then(Value::as_f64)
        .filter(|at| {
            at.is_finite()
                && *at >= now_millis.saturating_sub(3_600_000) as f64
                && *at <= (now_millis + 60_000) as f64
        })
        .map(|at| at as u64)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl TurnOwner {
    pub(in crate::server) async fn catchup_slice(&mut self, data: &Value) {
        let Some((rate, seq)) = slice_header(data) else {
            self.catchup = None;
            return;
        };
        if seq == 0 {
            self.catchup = Some(Catchup::begin(data, rate, now_millis()));
        }
        let Some(catchup) = &mut self.catchup else {
            return;
        };
        match catchup.append(data, rate, seq) {
            Ok(false) => {}
            Ok(true) => {
                let catchup = self.catchup.take().expect("catchup exists");
                self.queue_catchup(catchup).await;
            }
            Err(SliceError::Invalid) => self.catchup = None,
            Err(SliceError::TooLong) => {
                self.catchup = None;
                self.report("voice.catchup_too_long").await;
            }
        }
    }

    async fn queue_catchup(&mut self, catchup: Catchup) {
        let Some(target) = self.room.offline_target(&self.session) else {
            return;
        };
        self.catchups += 1;
        let row_id = format!("{}:user-catchup:{}", self.session, self.catchups);
        self.enqueue(RecognitionJob::Offline {
            target,
            row_id,
            pcm: catchup.pcm,
            rate: catchup.rate,
            truncated: catchup.truncated,
            time: catchup.time,
        })
        .await;
    }

    pub(super) async fn offline_result(
        &mut self,
        target: &VoiceTurn,
        row_id: &str,
        truncated: bool,
        time: Option<u64>,
        result: Recognition,
    ) {
        match result {
            Ok(Some(text)) => {
                let offline = if truncated { "truncated" } else { "buffered" };
                let _ = self
                    .events
                    .send(json!({"type":"voice-catchup-turn","data":{
                        "session_id":self.session,"history_id":row_id,"thread_id":target.thread_id,
                        "text":text,"offline":offline,"time":time}}))
                    .await;
                let _ = self
                    .room
                    .queue_offline_input(target, row_id, &text, offline, time);
            }
            Err(error) => self.report_failure(error, true).await,
            Ok(None) => {}
        }
    }
}
