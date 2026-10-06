//! The binary as shipped (no test overrides) answers a browser's WebRTC offer with gathered ICE candidates and no
//! STUN server to ask: on a machine with no route out, a call still gets its media path.

mod support;

use serde_json::Value;
use support::webrtc_peer::RtcBrowser;
use support::*;

#[tokio::test(flavor = "multi_thread")]
async fn an_offer_is_answered_with_gathered_candidates() {
    let root = tempfile::tempdir().unwrap();
    let mut core = Launch::new(root.path().join("core"))
        .arg("--launch-id")
        .arg("webrtc-answer")
        .start();
    let token = core.pair_local("Browser").await;
    let browser = core.join(&token, Value::Null).await;
    let rtc = RtcBrowser::new().await;
    assert!(
        rtc.offer.contains("a=candidate:"),
        "the offer carries candidates"
    );
    let (status, answer) = rtc.offer_to(&core, &token, &browser.session).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["type"], "answer");
    let sdp = answer["sdp"].as_str().unwrap();
    assert!(
        sdp.contains("a=candidate:"),
        "the answer carries gathered candidates: {sdp}"
    );
    rtc.close().await;
    browser.close().await;
    assert_eq!(core.stop(), 0);
    assert!(
        !core.data.join("core.json").exists(),
        "a clean exit removes the ready file"
    );
}
