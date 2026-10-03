//! Browser WebRTC audio offer/answer; the call socket remains the control path.

use std::sync::{Arc, Mutex, Weak};

use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use opus::{Channels, Decoder};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use webrtc::{
    media_stream::track_remote::{TrackRemote, TrackRemoteEvent},
    peer_connection::{
        register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
        PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState, RTCIceServer,
        RTCSessionDescription, Registry,
    },
};

use super::{failure, media::CallMedia, AppState, AuthenticatedDevice};

#[derive(Deserialize)]
pub struct Offer {
    session_id: String,
    sdp: String,
    #[serde(rename = "type")]
    kind: String,
}

fn enabled() -> bool {
    !matches!(
        std::env::var("SIDEVOICE_WEBRTC")
            .unwrap_or_else(|_| "on".into())
            .to_ascii_lowercase()
            .as_str(),
        "off" | "0" | "false"
    )
}

fn ice_urls() -> Vec<String> {
    match std::env::var("SIDEVOICE_STUN_URLS") {
        Ok(urls) => urls
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => vec!["stun:stun.l.google.com:19302".into()],
    }
}

pub async fn config() -> Json<Value> {
    let urls = ice_urls();
    Json(
        json!({"enabled":enabled(),"ice_servers":if enabled() && !urls.is_empty() { vec![json!({"urls":urls})] } else { vec![] }}),
    )
}

struct Handler {
    call: Weak<CallMedia>,
    generation: u64,
    gathered: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            if let Some(gathered) = self.gathered.lock().expect("gather lock").take() {
                let _ = gathered.send(());
            }
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let Some(ssrc) = track.ssrcs().await.first().copied() else {
            return;
        };
        if track
            .codec(ssrc)
            .await
            .is_none_or(|codec| !codec.mime_type.eq_ignore_ascii_case("audio/opus"))
        {
            return;
        }
        let call = self.call.clone();
        let generation = self.generation;
        tokio::spawn(async move {
            // libopus decodes straight to the required 16 kHz mono output. Its
            // decoder performs the codec's established resampling and downmix.
            let Ok(mut decoder) = Decoder::new(16_000, Channels::Mono) else {
                return;
            };
            let mut samples = [0_i16; 5760];
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                    let Some(call) = call.upgrade() else {
                        break;
                    };
                    let Ok(count) = decoder.decode(&packet.payload, &mut samples, false) else {
                        continue;
                    };
                    let mut pcm = Vec::with_capacity(count * 2);
                    for sample in &samples[..count] {
                        pcm.extend_from_slice(&sample.to_le_bytes());
                    }
                    call.feed_rtc(generation, pcm).await;
                }
            }
        });
    }
}

pub async fn offer(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    Json(offer): Json<Offer>,
) -> Response {
    if !enabled() {
        return failure(
            "voice.rtc_unavailable",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers,
        );
    }
    if offer.kind != "offer"
        || offer.session_id.is_empty()
        || offer.session_id.len() > 64
        || offer.sdp.is_empty()
        || offer.sdp.len() > 64_000
    {
        return failure(
            "room.request_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        );
    }
    if !state.room.owns_session(&offer.session_id, &device.0) {
        return failure("room.browser_absent", StatusCode::CONFLICT, &headers);
    }
    let call = state
        .media
        .lock()
        .expect("media lock")
        .get(&offer.session_id)
        .cloned();
    let Some(call) = call else {
        return failure("room.browser_absent", StatusCode::CONFLICT, &headers);
    };
    let generation = call.replace_rtc().await;
    let (gathered_tx, gathered_rx) = oneshot::channel();
    let handler = Arc::new(Handler {
        call: Arc::downgrade(&call),
        generation,
        gathered: Mutex::new(Some(gathered_tx)),
    });
    let mut engine = MediaEngine::default();
    if engine.register_default_codecs().is_err() {
        return failure(
            "voice.rtc_answer_failed",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        );
    }
    let registry = match register_default_interceptors(Registry::new(), &mut engine) {
        Ok(registry) => registry,
        Err(_) => {
            return failure(
                "voice.rtc_answer_failed",
                StatusCode::UNPROCESSABLE_ENTITY,
                &headers,
            )
        }
    };
    let urls = ice_urls();
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(if urls.is_empty() {
            vec![]
        } else {
            vec![RTCIceServer {
                urls,
                ..Default::default()
            }]
        })
        .build();
    let peer = match PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_udp_addrs(vec!["0.0.0.0:0".to_owned(), "127.0.0.1:0".to_owned()])
        .build()
        .await
    {
        Ok(peer) => Arc::new(peer) as Arc<dyn PeerConnection>,
        Err(_) => {
            return failure(
                "voice.rtc_answer_failed",
                StatusCode::UNPROCESSABLE_ENTITY,
                &headers,
            )
        }
    };
    let answer = async {
        let offer = RTCSessionDescription::offer(offer.sdp)
            .map_err(|error| format!("offer_parse: {error}"))?;
        peer.set_remote_description(offer).await
            .map_err(|error| format!("remote_description: {error}"))?;
        let answer = peer.create_answer(None).await
            .map_err(|error| format!("create_answer: {error}"))?;
        peer.set_local_description(answer).await
            .map_err(|error| format!("local_description: {error}"))?;
        gathered_rx.await.map_err(|error| format!("ice_gathering: {error}"))?;
        peer.local_description().await.ok_or_else(|| "gathered_description_missing".to_owned())
    };
    let answer = match tokio::time::timeout(std::time::Duration::from_secs(20), answer).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(error)) => {
            eprintln!("WebRTC answer failed: {error}");
            let _ = peer.close().await;
            return failure("voice.rtc_answer_failed", StatusCode::UNPROCESSABLE_ENTITY, &headers);
        }
        Err(_) => {
            eprintln!("WebRTC answer failed: gathered ICE timed out");
            let _ = peer.close().await;
            return failure("voice.rtc_answer_failed", StatusCode::UNPROCESSABLE_ENTITY, &headers);
        }
    };
    call.set_rtc(generation, peer).await;
    Json(json!({"sdp":answer.sdp,"type":"answer"})).into_response()
}
