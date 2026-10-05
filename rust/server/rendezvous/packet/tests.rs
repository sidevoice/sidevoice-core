use super::*;

#[test]
fn nested_binary_payload_keeps_envelope_and_attachment_order() {
    let raw = serde_json::json!({"channel":"c-1","data":{"voice":{"_placeholder":true,"num":1},"other":[{"_placeholder":true,"num":0}]}});
    let attachments = vec![b"first".to_vec(), b"second".to_vec()];
    let decoded = decode(raw.clone(), &attachments).unwrap();
    let mut encoded = Vec::new();
    let rebuilt = encode(decoded.clone(), &mut encoded);
    assert_eq!(decode(rebuilt, &encoded), Some(decoded));
    assert_eq!(encoded.len(), attachments.len());
}

#[test]
fn missing_attachment_is_rejected() {
    let raw = serde_json::json!({"body":{"_placeholder":true,"num":2}});
    assert!(decode(raw, &[b"only".to_vec()]).is_none());
}
