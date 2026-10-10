use super::*;

#[test]
fn filters_other_sessions_and_keeps_the_response_shape() {
    let marks = [
        Mark {
            session_id: "own",
            thread_id: "thread",
            revision: 2,
            utterance_id: None,
            event: Event::Queued,
            at_micros: 1_000_000,
        },
        Mark {
            session_id: "own",
            thread_id: "thread",
            revision: 2,
            utterance_id: None,
            event: Event::Read,
            at_micros: 1_250_000,
        },
        Mark {
            session_id: "own",
            thread_id: "thread",
            revision: 2,
            utterance_id: Some("reply"),
            event: Event::ReplyReceived,
            at_micros: 1_500_000,
        },
        Mark {
            session_id: "other",
            thread_id: "thread",
            revision: 2,
            utterance_id: Some("reply"),
            event: Event::ReplyDispatched,
            at_micros: 1_550_000,
        },
    ];
    let replies = [
        Reply {
            session_id: "own",
            thread_id: "thread",
            revision: 2,
            utterance_id: "reply",
            status: "received",
            input_ms: &[
                Duration {
                    name: "audio_ms",
                    milliseconds: 123.456,
                },
                Duration {
                    name: "transcript",
                    milliseconds: 999.0,
                },
            ],
            provider_ms: &[],
            browser_ms: &[],
        },
        Reply {
            session_id: "other",
            thread_id: "thread",
            revision: 2,
            utterance_id: "secret",
            status: "received",
            input_ms: &[],
            provider_ms: &[],
            browser_ms: &[],
        },
    ];
    let result = snapshot("own", "en", &marks, &replies);
    assert_eq!(result["replies"].as_array().unwrap().len(), 1);
    assert_eq!(result["replies"][0]["input_ms"], json!({"audio_ms":123.46}));
    assert_eq!(
        result["replies"][0]["server_ms"]["input_queued_to_reply_received_ms"],
        500.0
    );
    assert_eq!(
        result["replies"][0]["server_ms"]["read_to_reply_received_ms"],
        250.0
    );
    assert!(result["replies"][0]["server_ms"]
        .get("reply_received_to_dispatched_ms")
        .is_none());
    assert!(!result.to_string().contains("secret"));
    assert_eq!(
        snapshot("own", "fr", &marks, &replies)["notes"],
        result["notes"]
    );
}
