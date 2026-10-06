//! A browser's WebRTC end of a call: it offers one Opus microphone track and, once connected, sends PCM through
//! it in real time, encoded the way a browser's encoder would.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use opus::{Application, Channels, Encoder};
use rtc::ice::mdns::MulticastDnsMode;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState,
    RTCPeerConnectionState, RTCSessionDescription, Registry, SettingEngineBuilder,
};

use super::Core;

struct Events {
    gathered: Mutex<Option<oneshot::Sender<()>>>,
    connected: watch::Sender<bool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            if let Some(gathered) = self.gathered.lock().unwrap().take() {
                let _ = gathered.send(());
            }
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            self.connected.send_replace(true);
        }
    }
}

pub struct RtcBrowser {
    peer: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
    connected: watch::Receiver<bool>,
    /// The offer this browser sent, gathered.
    pub offer: String,
}

impl RtcBrowser {
    /// A peer with one microphone track, its offer gathered with numeric host candidates on loopback.
    pub async fn new() -> Self {
        let mut engine = MediaEngine::default();
        engine.register_default_codecs().expect("default codecs");
        let registry =
            register_default_interceptors(Registry::new(), &mut engine).expect("interceptors");
        let (gathered_tx, gathered) = oneshot::channel();
        let (connected_tx, connected) = watch::channel(false);
        let events = Arc::new(Events {
            gathered: Mutex::new(Some(gathered_tx)),
            connected: connected_tx,
        });
        let peer: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(RTCConfigurationBuilder::new().build())
                .with_media_engine(engine)
                .with_interceptor_registry(registry)
                .with_setting_engine(
                    SettingEngineBuilder::new()
                        .with_multicast_dns_mode(MulticastDnsMode::Disabled)
                        .build(),
                )
                .with_handler(events)
                .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
                .build()
                .await
                .expect("a peer connection"),
        );
        let ssrc = rand::random::<u32>();
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "browser".to_owned(),
                    "microphone".to_owned(),
                    "microphone".to_owned(),
                    RtpCodecKind::Audio,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(ssrc),
                            ..Default::default()
                        },
                        codec: RTCRtpCodec {
                            mime_type: MIME_TYPE_OPUS.to_owned(),
                            clock_rate: 48_000,
                            channels: 2,
                            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                            rtcp_feedback: vec![],
                        },
                        ..Default::default()
                    }],
                ),
            )
            .expect("an Opus track"),
        );
        let sender = peer
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
            .await
            .expect("the track is added");
        let offer = peer.create_offer(None).await.expect("an offer");
        peer.set_local_description(offer)
            .await
            .expect("the offer is set");
        tokio::time::timeout(Duration::from_secs(10), gathered)
            .await
            .expect("ICE gathering completes")
            .expect("gathering reported");
        let offer = peer
            .local_description()
            .await
            .expect("a local description")
            .sdp;
        let payload_type = sender
            .get_parameters()
            .await
            .expect("sender parameters")
            .rtp_parameters
            .codecs
            .first()
            .map(|codec| codec.payload_type)
            .unwrap_or(111);
        Self {
            peer,
            track,
            ssrc,
            payload_type,
            connected,
            offer,
        }
    }

    /// Sends the offer to the core for `session`; the core's answer (`status`, and the answer body).
    pub async fn offer_to(&self, core: &Core, token: &str, session: &str) -> (u16, Value) {
        let reply = core
            .post(
                "/api/presentation/rtc/offer",
                json!({"session_id": session, "type": "offer", "sdp": self.offer}),
            )
            .token(token)
            .send()
            .await;
        let body = if reply.body.is_empty() {
            Value::Null
        } else {
            reply.json()
        };
        (reply.status, body)
    }

    /// Offers, applies the answer and waits for the connection.
    pub async fn connect(&self, core: &Core, token: &str, session: &str) {
        let (status, answer) = self.offer_to(core, token, session).await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["type"], "answer");
        let sdp = answer["sdp"].as_str().expect("an answer SDP").to_owned();
        self.peer
            .set_remote_description(RTCSessionDescription::answer(sdp).expect("a valid answer"))
            .await
            .expect("the answer is applied");
        let mut connected = self.connected.clone();
        tokio::time::timeout(Duration::from_secs(15), connected.wait_for(|up| *up))
            .await
            .expect("the WebRTC connection comes up")
            .expect("connection state reported");
    }

    /// Sends 16 kHz mono PCM through the track, 20 ms per Opus packet, in real time.
    pub async fn speak(&self, pcm: &[u8]) {
        let mut encoder =
            Encoder::new(16_000, Channels::Mono, Application::Voip).expect("an Opus encoder");
        let samples: Vec<i16> = pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect();
        let started = Instant::now();
        let mut packet = vec![0_u8; 1500];
        for (index, frame) in samples.chunks(320).enumerate() {
            let mut frame = frame.to_vec();
            frame.resize(320, 0);
            let size = encoder.encode(&frame, &mut packet).expect("an Opus packet");
            let sample = Sample {
                data: bytes::Bytes::copy_from_slice(&packet[..size]),
                duration: Duration::from_millis(20),
                ..Sample::new(Instant::now())
            };
            let _ = self
                .track
                .sample_writer(self.ssrc, self.payload_type)
                .write_sample(&sample)
                .await;
            let due = started + Duration::from_millis(20 * (index as u64 + 1));
            tokio::time::sleep_until(due.into()).await;
        }
    }

    pub async fn close(&self) {
        let _ = self.peer.close().await;
    }
}
