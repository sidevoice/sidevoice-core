//! `relay.http`: one room request, replayed against this Core over loopback.

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, ORIGIN};
use reqwest::{RequestBuilder, Response};
use serde_json::json;
use url::Url;

use super::super::packet::Part;
use super::answer::http_error;
use super::path::relayable;
use super::{loopback_origin, query, text};

const METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
const MAX_BODY: usize = 8 * 1024 * 1024;

pub(super) async fn forward(client: &reqwest::Client, base: &Url, data: Part) -> Part {
    match request(client, base, &data) {
        Ok(request) => send(request).await,
        Err(answer) => answer,
    }
}

fn request(client: &reqwest::Client, base: &Url, data: &Part) -> Result<RequestBuilder, Part> {
    let not_found = || http_error(404, "request.not_found");
    let path = text(data, "path");
    let method = data
        .get("method")
        .and_then(Part::text)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    if !relayable(base, path, crate::server::local_only) || !METHODS.contains(&method.as_str()) {
        return Err(not_found());
    }
    let mut url = base.join(path).map_err(|_| not_found())?;
    if let Some(query) = query(data) {
        url.set_query(Some(query));
    }
    let headers = headers(base, data);
    let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| not_found())?;
    with_body(
        client.request(method, url).headers(headers),
        data.get("body"),
    )
}

/// Forward only these headers. When the room sent an origin, the loopback
/// request carries this Core's own instead.
fn headers(base: &Url, data: &Part) -> HeaderMap {
    let told = data.get("headers");
    let mut headers = HeaderMap::new();
    for (name, header) in [
        ("content-type", CONTENT_TYPE),
        ("accept", ACCEPT),
        ("authorization", AUTHORIZATION),
    ] {
        if let Some(value) = told.and_then(|v| v.get(name)).and_then(Part::text) {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(header, value);
            }
        }
    }
    if told
        .and_then(|v| v.get("origin"))
        .and_then(Part::text)
        .is_some()
    {
        if let Some(origin) = loopback_origin(base) {
            headers.insert(ORIGIN, origin);
        }
    }
    headers
}

fn with_body(request: RequestBuilder, body: Option<&Part>) -> Result<RequestBuilder, Part> {
    match body {
        Some(Part::Binary(bytes)) if bytes.len() <= MAX_BODY => Ok(request.body(bytes.clone())),
        Some(Part::Text(text)) if text.len() <= MAX_BODY => Ok(request.body(text.clone())),
        Some(Part::Binary(_) | Part::Text(_)) => Err(http_error(502, "relay.node_unavailable")),
        _ => Ok(request),
    }
}

async fn send(request: RequestBuilder) -> Part {
    let Ok(answer) = request.send().await else {
        return http_error(502, "relay.node_unavailable");
    };
    let status = answer.status().as_u16();
    let content_type = answer
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let Some(body) = read_body(answer).await else {
        return http_error(502, "relay.node_unavailable");
    };
    Part::object([
        ("status", Part::json(json!(status))),
        (
            "headers",
            Part::object([("content-type", Part::Text(content_type))]),
        ),
        ("body", Part::Binary(body)),
    ])
}

async fn read_body(mut answer: Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        match answer.chunk().await {
            Ok(Some(chunk)) if body.len().saturating_add(chunk.len()) <= MAX_BODY => {
                body.extend_from_slice(&chunk)
            }
            Ok(None) => return Some(body),
            _ => return None,
        }
    }
}
