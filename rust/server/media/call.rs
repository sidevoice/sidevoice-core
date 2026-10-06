//! The call's single microphone source: socket or WebRTC, feeding one detector.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use serde_json::Value;
use tokio::sync::mpsc;
use webrtc::peer_connection::PeerConnection;

use super::transcripts::DeviceTranscripts;
use crate::{
    pipeline::{CallDetector, CallFrame},
    types::CallSettings,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::server) enum Source {
    Socket,
    WebRtc,
}

pub(in crate::server) struct CallMedia {
    detector: CallDetector,
    source: Mutex<Source>,
    transcripts: DeviceTranscripts,
    rtc_generation: AtomicU64,
    rtc: Mutex<Option<Arc<dyn PeerConnection>>>,
    focus_tx: mpsc::Sender<()>,
    playing_uid: Mutex<Option<String>>,
}

type MediaStart = (
    Arc<CallMedia>,
    mpsc::Receiver<CallFrame>,
    mpsc::Receiver<()>,
);

impl CallMedia {
    pub(in crate::server) fn start(settings: &CallSettings) -> Result<MediaStart, String> {
        let (detector, events) = CallDetector::start(settings)?;
        let (focus_tx, focus_rx) = mpsc::channel(8);
        Ok((
            Arc::new(Self {
                detector,
                source: Mutex::new(Source::Socket),
                transcripts: DeviceTranscripts::default(),
                rtc_generation: AtomicU64::new(0),
                rtc: Mutex::new(None),
                focus_tx,
                playing_uid: Mutex::new(None),
            }),
            events,
            focus_rx,
        ))
    }

    pub(in crate::server) fn select(&self, source: Source) {
        *self.source.lock().expect("source lock") = source;
    }

    pub(in crate::server) async fn focus_changed(&self) {
        let _ = self.focus_tx.send(()).await;
    }

    pub(in crate::server) async fn feed(&self, source: Source, pcm: Vec<u8>) {
        if *self.source.lock().expect("source lock") == source {
            let _ = self.detector.feed(pcm).await;
        }
    }

    pub(in crate::server) async fn feed_rtc(&self, generation: u64, pcm: Vec<u8>) {
        if self.rtc_generation.load(Ordering::Acquire) == generation {
            self.feed(Source::WebRtc, pcm).await;
        }
    }

    /// Closes any current peer and returns the generation a new peer must present.
    pub(in crate::server) async fn replace_rtc(&self) -> u64 {
        let generation = self.rtc_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let prior = self.rtc.lock().expect("rtc lock").take();
        if let Some(prior) = prior {
            let _ = prior.close().await;
        }
        generation
    }

    pub(in crate::server) async fn set_rtc(&self, generation: u64, peer: Arc<dyn PeerConnection>) {
        if self.rtc_generation.load(Ordering::Acquire) == generation {
            self.rtc.lock().expect("rtc lock").replace(peer);
        } else {
            let _ = peer.close().await;
        }
    }

    pub(in crate::server) async fn close_rtc(&self) {
        self.rtc_generation.fetch_add(1, Ordering::AcqRel);
        let peer = self.rtc.lock().expect("rtc lock").take();
        if let Some(peer) = peer {
            let _ = peer.close().await;
        }
    }

    pub(in crate::server) fn transcript(&self, data: &Value, error: bool, session: &str) {
        self.transcripts.resolve(data, error, session);
    }

    pub(super) fn transcripts(&self) -> &DeviceTranscripts {
        &self.transcripts
    }

    pub(in crate::server) fn close(&self) {
        self.transcripts.clear();
    }

    pub(super) async fn listening_bar(&self, playing: bool) {
        if !playing {
            self.playing_uid.lock().expect("playing lock").take();
        }
        self.detector.listening_bar(playing).await;
    }

    pub(super) async fn reset_detector(&self) {
        self.detector.reset().await;
    }

    /// Raises the listening bar while one of our utterances plays, lowering it when that one ends.
    pub(in crate::server) async fn admitted_receipt(&self, uid: &str, status: &str) {
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
