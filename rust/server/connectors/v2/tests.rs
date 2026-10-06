use serde_json::{json, Value};

use super::events::readable;
use super::refusal;
use crate::messages::{render, LocalizedMessage};

#[test]
fn a_refused_credential_or_protocol_says_why() {
    let paired = |_: &str, _: &str, _: &Value| true;
    let unknown = |_: &str, _: &str, _: &Value| false;
    let refused = render(&LocalizedMessage::new("connector.credential_refused"), "en");
    assert!(!refused.contains("connector.credential_refused"));
    assert_eq!(refusal(None, paired), Some(refused.clone()));
    assert_eq!(
        refusal(Some(&json!("token")), paired),
        Some(refused.clone())
    );
    let current = json!({"connector_id":"c","token":"t","protocol":2});
    assert_eq!(refusal(Some(&current), unknown), Some(refused));
    assert_eq!(refusal(Some(&current), paired), None);
    let old = refusal(
        Some(&json!({"connector_id":"c","token":"t","protocol":1})),
        paired,
    )
    .unwrap();
    assert!(
        old.contains("protocol 1")
            && old.contains("speaks 2")
            && !old.contains("connector.protocol"),
        "{old}"
    );
}

#[test]
fn v2_errors_are_sentences_and_other_fields_stay() {
    let answer =
        readable(json!({"status":"rejected","error":"room.binding_foreign","event_id":"e"}));
    assert_eq!(answer["error"], "Unknown or foreign binding.");
    assert_eq!(answer["status"], "rejected");
    assert_eq!(answer["event_id"], "e");
    let registered = json!({"binding_id":"b","thread":"t"});
    assert_eq!(readable(registered.clone()), registered);
}
