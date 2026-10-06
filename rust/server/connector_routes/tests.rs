use axum::http::StatusCode;
use serde_json::{json, Value};

use super::agent_id_valid;
use super::answer::{host_agent_response, safe_connector_params, valid_connector_error_key};
use crate::control::room::PeerError;

#[test]
fn agent_ids_are_bounded_lowercase_slugs() {
    for id in ["codex", "0-agent", "a.b_c-d", "a".repeat(100).as_str()] {
        assert!(agent_id_valid(id), "{id}");
    }
    for id in [
        "",
        "-agent",
        "Agent",
        "a/b",
        "a b",
        "a".repeat(101).as_str(),
    ] {
        assert!(!agent_id_valid(id), "{id}");
    }
}

#[test]
fn connector_error_keys_are_dotted_lowercase_words() {
    for key in ["connector-error", "agent.not_found", "a1"] {
        assert!(valid_connector_error_key(key), "{key}");
    }
    for key in ["", "1agent", "agent.", "agent..x", "agent-_x", "Agent"] {
        assert!(!valid_connector_error_key(key), "{key}");
    }
}

#[test]
fn connector_params_drop_raw_output_and_bound_size() {
    let long = "x".repeat(600);
    let params = json!({
        "name": "codex",
        "count": 3,
        "ratio": 0.5,
        "stdout": "secret",
        "errorDetail": "secret",
        "note": long,
        "list": (0..30).collect::<Vec<_>>(),
    });
    let clean = safe_connector_params(&params, 0).unwrap();
    assert_eq!(clean["name"], "codex");
    assert_eq!(clean["count"], 3);
    assert!(clean.get("ratio").is_none());
    assert!(clean.get("stdout").is_none());
    assert!(clean.get("errorDetail").is_none());
    assert_eq!(clean["note"].as_str().unwrap().len(), 500);
    assert_eq!(clean["list"].as_array().unwrap().len(), 24);
    let deep = json!({"a":{"b":{"c":{"d":{"e":{"f":1}}}}}});
    assert_eq!(
        safe_connector_params(&deep, 0).unwrap(),
        json!({"a":{"b":{"c":{"d":{}}}}})
    );
}

async fn body(response: axum::response::Response) -> Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn connector_answers_map_to_device_responses() {
    let refused = host_agent_response(Some(Ok(
        json!({"error":{"key":"Bad Key","params":{"id":"a","output":"x"},"message":"no"}}),
    )));
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(
        body(refused).await,
        json!({"error":{"key":"connector-error","params":{"id":"a"},"message":"no"}})
    );
    let listed = host_agent_response(Some(Ok(json!({"agents":[],"custom":{},"extra":1}))));
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(
        body(listed).await,
        json!({"agents":[],"scanned_at":null,"custom":{}})
    );
    assert_eq!(
        host_agent_response(Some(Ok(json!({"agents":[]})))).status(),
        StatusCode::BAD_GATEWAY
    );
    // A connector that could not answer is 502; only the routes' own deadline is a timeout (504).
    let unavailable = host_agent_response(Some(Err(PeerError)));
    assert_eq!(unavailable.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        body(unavailable).await,
        json!({"key":"connector-unavailable"})
    );
    let timeout = host_agent_response(None);
    assert_eq!(timeout.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body(timeout).await, json!({"key":"connector-timeout"}));
}
