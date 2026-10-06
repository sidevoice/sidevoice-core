//! Builds the answering peer connection and negotiates its gathered answer.

use std::{sync::Arc, time::Duration};

#[cfg(feature = "hosted-fixtures")]
use webrtc::peer_connection::SettingEngineBuilder;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    RTCConfigurationBuilder, RTCIceServer, RTCSessionDescription, Registry,
};

use super::handler::Handler;

/// The refusal key for every failure to build a peer or produce its answer.
pub(super) const ANSWER_FAILED: &str = "voice.rtc_answer_failed";
const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// Returns the peer, or the log line explaining why it could not be built.
pub(super) async fn build(
    handler: Arc<Handler>,
    urls: Vec<String>,
) -> Result<Arc<dyn PeerConnection>, String> {
    let mut engine = MediaEngine::default();
    engine
        .register_default_codecs()
        .map_err(|error| format!("WebRTC codec registration failed: {error}"))?;
    let registry = register_default_interceptors(Registry::new(), &mut engine)
        .map_err(|error| format!("WebRTC interceptor registration failed: {error}"))?;
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
    let builder = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_udp_addrs(vec!["0.0.0.0:0".to_owned(), "127.0.0.1:0".to_owned()]);
    // The hosted network has no multicast route; aiortc offers numeric host candidates.
    #[cfg(feature = "hosted-fixtures")]
    let builder = builder.with_setting_engine(
        SettingEngineBuilder::new()
            .with_multicast_dns_mode(::rtc::ice::mdns::MulticastDnsMode::Disabled)
            .build(),
    );
    match builder.build().await {
        Ok(peer) => Ok(Arc::new(peer) as Arc<dyn PeerConnection>),
        Err(error) => Err(format!("WebRTC peer construction failed: {error}")),
    }
}

/// Answers the offer once ICE gathering completes, so the answer carries every candidate.
pub(super) async fn answer(
    peer: &Arc<dyn PeerConnection>,
    sdp: String,
    gathered: tokio::sync::oneshot::Receiver<()>,
) -> Result<RTCSessionDescription, String> {
    let answer = async {
        let offer =
            RTCSessionDescription::offer(sdp).map_err(|error| format!("offer_parse: {error}"))?;
        peer.set_remote_description(offer)
            .await
            .map_err(|error| format!("remote_description: {error}"))?;
        let answer = peer
            .create_answer(None)
            .await
            .map_err(|error| format!("create_answer: {error}"))?;
        peer.set_local_description(answer)
            .await
            .map_err(|error| format!("local_description: {error}"))?;
        gathered
            .await
            .map_err(|error| format!("ice_gathering: {error}"))?;
        peer.local_description()
            .await
            .ok_or_else(|| "gathered_description_missing".to_owned())
    };
    tokio::time::timeout(ANSWER_TIMEOUT, answer)
        .await
        .unwrap_or_else(|_| Err("gathered ICE timed out".to_owned()))
}
