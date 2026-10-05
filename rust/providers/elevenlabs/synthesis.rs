//! Text-to-speech synthesis over the SDK's audio and timestamped streams, with boundary timings.

use super::{
    wire::{
        decode_timestamp_stream, map_elevenlabs_error, mime_type, output_format_from_str,
        stream_error,
    },
    ElevenLabsTts,
};
use crate::providers::{ProviderError, ProviderErrorKind};
use elevenlabs_sdk::{
    error::ElevenLabsError,
    types::{TextToSpeechRequest, VoiceSettings},
};
use futures_util::{Stream, StreamExt};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};
use tokio::time::Instant;

#[derive(Clone, Debug, PartialEq)]
pub struct CloudSpeech {
    pub audio: Vec<u8>,
    pub mime_type: String,
    pub alignment: Option<Value>,
    pub timings_ms: Map<String, Value>,
}

impl ElevenLabsTts {
    pub async fn synthesize(
        &self,
        text: &str,
        model: &str,
        voice: &str,
        speed: f64,
        with_timestamps: bool,
        output_format: &str,
    ) -> Result<CloudSpeech, ProviderError> {
        if !speed.is_finite() || voice.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidConfiguration,
                None,
            ));
        }
        let output_format = output_format_from_str(output_format)?;
        let request = speech_request(text, model, speed);
        let encoded_voice = utf8_percent_encode(voice, NON_ALPHANUMERIC).to_string();
        let service = self.client.text_to_speech();
        let started = Instant::now();
        let mut timings_ms = Map::new();
        let (audio, alignment) = if with_timestamps {
            let stream = service
                .convert_stream_with_timestamps(&encoded_voice, &request, Some(output_format), None)
                .await
                .map_err(map_elevenlabs_error)?;
            let body = read_timed_body(stream, started, &mut timings_ms).await?;
            decode_timestamp_stream(&body)?
        } else {
            let stream = service
                .convert_stream(&encoded_voice, &request, Some(output_format), None)
                .await
                .map_err(map_elevenlabs_error)?;
            (
                read_timed_body(stream, started, &mut timings_ms).await?,
                None,
            )
        };
        if audio.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::MalformedResponse,
                None,
            ));
        }
        Ok(CloudSpeech {
            audio,
            mime_type: mime_type(output_format),
            alignment,
            timings_ms,
        })
    }
}

fn speech_request(text: &str, model: &str, speed: f64) -> TextToSpeechRequest {
    TextToSpeechRequest {
        text: text.to_owned(),
        model_id: Some(model.to_owned()),
        voice_settings: Some(VoiceSettings {
            stability: None,
            similarity_boost: None,
            style: None,
            use_speaker_boost: None,
            speed: Some(speed.clamp(0.7, 1.2)),
        }),
        ..TextToSpeechRequest::new("")
    }
}

/// Drains a response body stream whose headers just arrived, recording the boundary timings.
async fn read_timed_body<S, B, E>(
    mut stream: S,
    started: Instant,
    timings_ms: &mut Map<String, Value>,
) -> Result<Vec<u8>, ProviderError>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: Into<ElevenLabsError>,
{
    // These are observations at the SDK stream boundary: method return after successful
    // headers, first nonempty Bytes item, and stream exhaustion. hpx may buffer or coalesce
    // body data differently from Python aiohttp's iter_any(), so first-byte parity is not
    // guaranteed even though the public timing key and observed event are retained.
    record_elapsed(timings_ms, "request_to_headers_ms", started);
    let mut body = Vec::new();
    let mut first_chunk_recorded = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(stream_error)?;
        let chunk = chunk.as_ref();
        if !chunk.is_empty() {
            if !first_chunk_recorded {
                first_chunk_recorded = true;
                record_elapsed(timings_ms, "request_to_first_chunk_ms", started);
            }
            body.extend_from_slice(chunk);
        }
    }
    record_elapsed(timings_ms, "request_to_complete_ms", started);
    Ok(body)
}

fn record_elapsed(timings_ms: &mut Map<String, Value>, key: &str, started: Instant) {
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    timings_ms.insert(key.to_owned(), json!(elapsed_ms));
}
