//! Error answers in the shape the room expects for each request kind.

use serde_json::json;

use super::super::packet::Part;
use crate::messages::{render, LocalizedMessage};

pub(super) fn http_error(status: u16, key: &str) -> Part {
    let detail = render(&LocalizedMessage::new(key), "en");
    Part::object([
        ("status", Part::json(json!(status))),
        (
            "headers",
            Part::object([("content-type", Part::Text("application/json".into()))]),
        ),
        (
            "body",
            Part::Binary(json!({"detail": detail}).to_string().into_bytes()),
        ),
    ])
}

pub(super) fn open_error(status: u16, key: &str) -> Part {
    Part::object([
        ("ok", Part::Bool(false)),
        ("status", Part::json(json!(status))),
        (
            "detail",
            Part::Text(render(&LocalizedMessage::new(key), "en")),
        ),
    ])
}
