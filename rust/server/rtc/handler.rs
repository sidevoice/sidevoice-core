//! Peer events for one offer: ICE gathering completion and the browser's Opus track.

use std::sync::{Arc, Mutex, Weak};

use opus::{Channels, Decoder};
use tokio::sync::oneshot;
use webrtc::{
    media_stream::track_remote::{TrackRemote, TrackRemoteEvent},
    peer_connection::{PeerConnectionEventHandler, RTCIceGatheringState},
};

use crate::server::media::CallMedia;

/// Largest Opus frame: 120 ms at 48 kHz.
const MAX_FRAME_SAMPLES: usize = 5760;

pub(super) struct Handler {
    call: Weak<CallMedia>,
    generation: u64,
    gathered: Mutex<Option<oneshot::Sender<()>>>,
}

impl Handler {
    /// The receiver resolves once ICE gathering completes.
    pub(super) fn new(
        call: &Arc<CallMedia>,
        generation: u64,
    ) -> (Arc<Self>, oneshot::Receiver<()>) {
        let (gathered_tx, gathered_rx) = oneshot::channel();
        let handler = Arc::new(Self {
            call: Arc::downgrade(call),
            generation,
            gathered: Mutex::new(Some(gathered_tx)),
        });
        (handler, gathered_rx)
    }
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
        tokio::spawn(forward_opus(track, self.call.clone(), self.generation));
    }
}

/// Decodes the track into the call until the track or the call ends.
async fn forward_opus(track: Arc<dyn TrackRemote>, call: Weak<CallMedia>, generation: u64) {
    // libopus decodes straight to the required 16 kHz mono output. Its
    // decoder performs the codec's established resampling and downmix.
    let Ok(mut decoder) = Decoder::new(16_000, Channels::Mono) else {
        return;
    };
    let mut samples = [0_i16; MAX_FRAME_SAMPLES];
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
}
