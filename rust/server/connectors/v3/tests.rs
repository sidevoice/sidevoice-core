use super::frame::decode;

#[test]
fn minimum_i64_id_is_a_protocol_error() {
    for frame in [
        r#"{"jsonrpc":"2.0","id":-9223372036854775808,"method":"input.read","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":-9223372036854775808,"result":{}}"#,
    ] {
        assert_eq!(decode(frame).unwrap_err(), 1002);
    }
    assert!(decode(r#"{"jsonrpc":"2.0","id":-9007199254740991,"result":{}}"#).is_ok());
}

#[test]
fn frames_must_be_one_well_formed_call_or_response() {
    for frame in [
        r#"{"jsonrpc":"2.0","method":"input.read"}"#,
        r#"{"jsonrpc":"2.0","id":"a","method":"input.read","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":7,"error":{"code":1}}"#,
    ] {
        assert!(decode(frame).is_ok(), "{frame}");
    }
    for frame in [
        "[]",
        "not json",
        r#"{"jsonrpc":"1.0","method":"input.read"}"#,
        r#"{"jsonrpc":"2.0","method":""}"#,
        r#"{"jsonrpc":"2.0","method":"input.read","params":[]}"#,
        r#"{"jsonrpc":"2.0","method":"input.read","result":{}}"#,
        r#"{"jsonrpc":"2.0","id":"","method":"input.read"}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#,
        r#"{"jsonrpc":"2.0","id":1}"#,
        r#"{"jsonrpc":"2.0","result":{}}"#,
    ] {
        assert_eq!(decode(frame).unwrap_err(), 1002, "{frame}");
    }
    let oversized = format!(
        r#"{{"jsonrpc":"2.0","method":"{}"}}"#,
        "a".repeat(1024 * 1024)
    );
    assert_eq!(decode(&oversized).unwrap_err(), 1009);
}
