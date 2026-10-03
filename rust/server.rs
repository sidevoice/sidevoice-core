//! Minimum device trust surface on TCP and the same user's Unix socket.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{
    ws::{CloseFrame, Message, WebSocket},
    Extension, Path, Request, State, WebSocketUpgrade,
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::watch;
use url::Url;
use uuid::Uuid;

use crate::control::devices::{valid_nonce, DeviceRegistry, NodeIdentity};
use crate::control::room::Room;
use crate::messages::{render, LocalizedMessage};
use crate::runtime::API;
use crate::storage::PrivateDir;

mod connectors_v2;
mod connectors_v3;

pub struct AppState {
    pub dir: PrivateDir,
    pub room: Arc<Room>,
    pub identity: NodeIdentity,
    registry: Mutex<DeviceRegistry>,
    calls: Mutex<HashMap<String, Vec<watch::Sender<bool>>>>,
    launch_id: String,
    host: String,
    port: u16,
}

#[derive(Clone)]
struct AuthenticatedDevice(String);

impl AppState {
    pub fn new(
        dir: PrivateDir,
        identity: NodeIdentity,
        registry: DeviceRegistry,
        launch_id: String,
        host: String,
        port: u16,
        room: Arc<Room>,
    ) -> Self {
        Self {
            dir,
            room,
            identity,
            registry: Mutex::new(registry),
            calls: Mutex::new(HashMap::new()),
            launch_id,
            host,
            port,
        }
    }

    pub fn issue_code(&self) -> Value {
        let mut urls = vec![format!("http://127.0.0.1:{}", self.port)];
        for value in std::env::var("SIDEVOICE_PUBLIC_URLS")
            .unwrap_or_default()
            .split(',')
        {
            let value = value.trim().trim_end_matches('/');
            if !value.is_empty() && safe_url(value) && !urls.iter().any(|url| url == value) {
                urls.push(value.to_owned());
            }
        }
        self.registry.lock().expect("registry lock").issue_code(
            &self.identity,
            Some(&self.host),
            &urls,
        )
    }

    pub fn open_calls(&self) -> usize {
        self.calls
            .lock()
            .expect("calls lock")
            .values()
            .map(|senders| senders.iter().filter(|tx| tx.receiver_count() > 0).count())
            .sum()
    }

    pub fn authenticate_token(&self, token: &str) -> Option<String> {
        self.registry
            .lock()
            .expect("registry lock")
            .authenticate(token)
    }

    pub fn close_calls(&self, id: &str) {
        if let Some(senders) = self.calls.lock().expect("calls lock").remove(id) {
            for sender in senders {
                let _ = sender.send(true);
            }
        }
    }

    fn node(&self) -> Value {
        json!({"fingerprint": self.identity.fingerprint, "public_key": self.identity.public_key, "host": self.host})
    }
}

fn safe_url(value: &str) -> bool {
    let Ok(parsed) = Url::parse(value) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if parsed.scheme() == "https" {
        return true;
    }
    if parsed.scheme() != "http" {
        return false;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1") {
        return true;
    }
    std::env::var("SIDEVOICE_TRUSTED_CLUSTER_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry.starts_with('.') {
                host.ends_with(&entry)
            } else {
                host == entry
            }
        })
}

fn local_only(path: &str) -> bool {
    let mut decoded = path.to_owned();
    for _ in 0..2 {
        decoded = percent_decode_str(&decoded).decode_utf8_lossy().to_string();
    }
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => (),
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let normalized = format!("/{}", segments.join("/"));
    [
        "/api/local",
        "/api/device/local",
        "/api/connectors/link",
        "/api/connectors/v3",
    ]
    .iter()
    .any(|prefix| normalized == *prefix || normalized.starts_with(&format!("{prefix}/")))
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let origin = origin.trim_end_matches('/');
    let public_origin = std::env::var("VOICE_PUBLIC_ORIGIN").unwrap_or_default();
    let configured = std::env::var("SIDEVOICE_ALLOWED_ORIGINS").unwrap_or_default();
    let allowed = [
        "tauri://localhost",
        "http://tauri.localhost",
        "https://tauri.localhost",
    ]
    .into_iter()
    .chain(public_origin.split(',').map(str::trim))
    .chain(configured.split(',').map(str::trim))
    .any(|item| !item.is_empty() && item.trim_end_matches('/') == origin);
    if allowed {
        return true;
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let own = Url::parse(&format!("http://{host}")).ok();
    Url::parse(origin)
        .ok()
        .zip(own)
        .is_some_and(|(origin, own)| {
            origin.host_str() == own.host_str() && origin.port() == own.port()
        })
}

fn host_allowed(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let host = if value.starts_with('[') {
        value
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[')
    } else {
        value.split(':').next().unwrap_or("")
    }
    .to_ascii_lowercase();
    if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    if std::env::var("SIDEVOICE_ALLOWED_HOSTS")
        .unwrap_or_default()
        .split(',')
        .any(|item| item.trim().eq_ignore_ascii_case(&host))
    {
        return true;
    }
    let configured = std::env::var("SIDEVOICE_ALLOWED_ORIGINS").unwrap_or_default();
    let public_origin = std::env::var("VOICE_PUBLIC_ORIGIN").unwrap_or_default();
    configured
        .split(',')
        .chain(public_origin.split(','))
        .any(|origin| {
            Url::parse(origin.trim())
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|name| name.eq_ignore_ascii_case(&host))
        })
}

fn failure(key: &str, status: StatusCode, headers: &HeaderMap) -> Response {
    let language = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .unwrap_or("en");
    let message = render(
        &LocalizedMessage {
            key: key.to_owned(),
            params: Map::new(),
        },
        language,
    );
    let mut response = (status, Json(json!({"detail": message}))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

fn cors(response: &mut Response, headers: &HeaderMap) {
    if !origin_allowed(headers) {
        return;
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Origin"));
    }
}

async fn guard(
    State((state, local)): State<(Arc<AppState>, bool)>,
    mut request: Request,
    next: Next,
) -> Response {
    let headers = request.headers().clone();
    let path = request.uri().path().to_owned();
    if !host_allowed(&headers) {
        return failure(
            "request.host_invalid",
            StatusCode::MISDIRECTED_REQUEST,
            &headers,
        );
    }
    if local_only(&path) && (!local || headers.contains_key(header::ORIGIN)) {
        return failure("request.not_found", StatusCode::NOT_FOUND, &headers);
    }
    if request.method() == axum::http::Method::OPTIONS
        && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    {
        let mut response = StatusCode::NO_CONTENT.into_response();
        cors(&mut response, &headers);
        if response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        {
            if headers
                .get("access-control-request-private-network")
                .is_some_and(|value| value == "true")
            {
                response.headers_mut().insert(
                    "access-control-allow-private-network",
                    HeaderValue::from_static("true"),
                );
            }
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE"),
            );
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("content-type, accept, authorization"),
            );
            response.headers_mut().insert(
                header::ACCESS_CONTROL_MAX_AGE,
                HeaderValue::from_static("600"),
            );
        }
        return response;
    }
    let open = (request.method() == axum::http::Method::GET
        && matches!(path.as_str(), "/api/rendezvous" | "/api/device/identity"))
        || (request.method() == axum::http::Method::POST && path == "/api/device/pair")
        || (local && local_only(&path));
    let is_ws = request.method() == axum::http::Method::GET
        && path == "/api/presentation/ws"
        && headers
            .get(header::UPGRADE)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if !open && !is_ws {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let (scheme, token) = value.split_once(' ')?;
                scheme
                    .eq_ignore_ascii_case("bearer")
                    .then_some(token.trim())
                    .filter(|v| !v.is_empty())
            });
        let device = token.and_then(|token| {
            state
                .registry
                .lock()
                .expect("registry lock")
                .authenticate(token)
        });
        let Some(device) = device else {
            let mut response = failure("device.unpaired", StatusCode::UNAUTHORIZED, &headers);
            cors(&mut response, &headers);
            return response;
        };
        request.extensions_mut().insert(AuthenticatedDevice(device));
    }
    let mut response = next.run(request).await;
    cors(&mut response, &headers);
    response
}

pub fn router(state: Arc<AppState>, local: bool) -> Router {
    let mut router = Router::new()
        .route("/api/rendezvous", get(rendezvous))
        .route("/api/device/identity", get(identity))
        .route("/api/device/pair", post(pair))
        .route("/api/device/devices", get(devices))
        .route(
            "/api/device/devices/{device_id}",
            axum::routing::delete(revoke),
        )
        .route("/api/presentation/ws", get(call_socket));
    router = router
        .route("/api/presentation", get(presentation_state))
        .route("/api/presentation/admission", get(presentation_admission))
        .route("/api/presentation/history", get(presentation_history))
        .route(
            "/api/presentation/participants",
            get(presentation_participants),
        )
        .route("/api/presentation/select", post(presentation_select))
        .route("/api/presentation/leave", post(presentation_leave))
        .route("/api/presentation/close", post(presentation_close))
        .route("/api/presentation/text", post(presentation_text))
        .route(
            "/api/presentation/browser-receipt",
            post(presentation_receipt),
        )
        .route(
            "/api/presentation/client-error",
            post(presentation_client_error),
        )
        .route("/api/presentation/speak", post(presentation_speak))
        .route("/api/connectors", get(connector_listing))
        .route("/api/host/agents", get(host_agents))
        .route(
            "/api/host/agents/{agent_id}/{action}",
            post(host_agent_action),
        );
    if local {
        router = router
            .route("/api/local/health", get(health))
            .route("/api/device/local/pair", post(pair_local))
            .route("/api/device/local", axum::routing::delete(unpair_local));
        router = router.route("/api/connectors/v3", get(connector_v3));
    }
    let router =
        router
            .fallback(not_found)
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(
                (state.clone(), local),
                guard,
            ));
    if local {
        connectors_v2::layer(router, state)
    } else {
        router
    }
}

fn room_failure(error: crate::control::room::RoomError, headers: &HeaderMap) -> Response {
    failure(
        error.key,
        StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST),
        headers,
    )
}
fn payload(body: &axum::body::Bytes) -> Option<Value> {
    serde_json::from_slice(body).ok().filter(Value::is_object)
}
fn query(uri: &axum::http::Uri, name: &str) -> Option<String> {
    uri.query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    })
}
async fn presentation_state(
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
) -> Json<Value> {
    Json(state.room.snapshot(query(&uri, "session_id").as_deref()))
}
async fn presentation_admission(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<Value> {
    Json(
        state.room.admission(
            headers
                .get(header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("en"),
        ),
    )
}
async fn presentation_history(
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    Json(state.room.history(query(&uri, "thread_id").as_deref())).into_response()
}
async fn presentation_participants(
    State(state): State<Arc<AppState>>,
    uri: axum::http::Uri,
) -> Json<Value> {
    Json(json!({"participants":state.room.participants(query(&uri,"session_id").as_deref())}))
}
async fn presentation_select(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    match state.room.select(
        data["session_id"].as_str().unwrap_or(""),
        data["thread_id"].as_str().unwrap_or(""),
    ) {
        Ok(v) => Json(v).into_response(),
        Err(e) => room_failure(e, &headers),
    }
}
async fn presentation_leave(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    match state.room.deselect(
        data["session_id"].as_str().unwrap_or(""),
        data["binding_id"].as_str().unwrap_or(""),
    ) {
        Ok(v) => Json(v).into_response(),
        Err(e) => room_failure(e, &headers),
    }
}
async fn presentation_close(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    match state
        .room
        .close_channel(data["thread_id"].as_str().unwrap_or(""))
    {
        Ok((result, notify)) => {
            if let Some((peer, params)) = notify {
                let _ = peer.send("binding.close", params).await;
            }
            Json(result).into_response()
        }
        Err(e) => room_failure(e, &headers),
    }
}
async fn presentation_text(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    let text = data["text"].as_str().unwrap_or("");
    let mid = data["message_id"].as_str().unwrap_or("");
    if text.len() > 12000 || Uuid::parse_str(mid).is_err() {
        return failure(
            "room.request_invalid",
            StatusCode::UNPROCESSABLE_ENTITY,
            &headers,
        );
    }
    match state.room.send_text(
        text,
        data["session_id"].as_str().unwrap_or(""),
        data["thread_id"].as_str().unwrap_or(""),
        data["binding_id"].as_str().unwrap_or(""),
        mid,
    ) {
        Ok(v) => Json(v).into_response(),
        Err(e) => room_failure(e, &headers),
    }
}
async fn presentation_receipt(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    match state.room.receipt(
        data["session_id"].as_str().unwrap_or(""),
        data["utterance_id"].as_str().unwrap_or(""),
        data["revision"].as_u64().unwrap_or(u64::MAX),
        data["status"].as_str().unwrap_or(""),
    ) {
        Ok(v) => Json(v).into_response(),
        Err(e) => room_failure(e, &headers),
    }
}
async fn presentation_client_error(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    Json(state.room.report_client_error(&data)).into_response()
}
async fn presentation_speak(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    Json(state.room.publish(&data, false)).into_response()
}
async fn connector_listing(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    Json(json!({"connectors":state.room.paired_connectors(),"bindings":state.room.binding_views()}))
        .into_response()
}
async fn connector_v3(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| connectors_v3::run(state, socket))
        .into_response()
}
async fn host_agents(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let rescan = query(&uri, "rescan").unwrap_or_default();
    let force = match rescan.to_ascii_lowercase().as_str() {
        "" | "0" | "false" => false,
        "1" | "true" => true,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"key":"invalid-rescan"})),
            )
                .into_response()
        }
    };
    let watch = query(&uri, "watch");
    if watch.as_ref().is_some_and(|s| {
        s.is_empty()
            || s.len() > 100
            || !s
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    }) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"key":"invalid-agent-id"})),
        )
            .into_response();
    }
    let Some(peer) = state.room.connector_peer() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"key":"no-connector"})),
        )
            .into_response();
    };
    let mut params = json!({"rescan":force});
    if let Some(watch) = watch {
        params["watch"] = json!(watch)
    }
    host_agent_response(
        peer.request("agents.list", params, std::time::Duration::from_secs(20))
            .await,
    )
}
fn agent_id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}
fn host_agent_response(answer: Result<Value, crate::control::room::PeerError>) -> Response {
    match answer {
        Ok(v) if v["agents"].is_array() && v["custom"].is_object() => Json(
            json!({"agents":v["agents"],"scanned_at":v.get("scanned_at"),"custom":v["custom"]}),
        )
        .into_response(),
        Ok(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"key":"invalid-connector-response"})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({"key":"connector-timeout"})),
        )
            .into_response(),
    }
}
async fn host_agent_action(
    State(state): State<Arc<AppState>>,
    Path((agent_id, action)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    if !agent_id_valid(&agent_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"key":"invalid-agent-id"})),
        )
            .into_response();
    }
    if !["connect", "disconnect", "dismiss"].contains(&action.as_str()) {
        return failure("request.not_found", StatusCode::NOT_FOUND, &headers);
    }
    let Some(peer) = state.room.connector_peer() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"key":"no-connector"})),
        )
            .into_response();
    };
    host_agent_response(
        peer.request(
            &format!("agents.{action}"),
            json!({"id":agent_id}),
            std::time::Duration::from_secs(20),
        )
        .await,
    )
}

async fn not_found(headers: HeaderMap) -> Response {
    failure("request.not_found", StatusCode::NOT_FOUND, &headers)
}

async fn rendezvous(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"kind": "node", "fingerprint": state.identity.fingerprint, "api": API}))
}

async fn identity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let nonce = uri
        .query()
        .and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(name, _)| name == "nonce")
                .map(|(_, value)| value.into_owned())
        })
        .unwrap_or_default();
    if !valid_nonce(&nonce) {
        return failure("device.nonce_invalid", StatusCode::BAD_REQUEST, &headers);
    }
    Json(
        json!({"fingerprint": state.identity.fingerprint, "public_key": state.identity.public_key,
        "signature": state.identity.sign(&nonce)}),
    )
    .into_response()
}

#[derive(Deserialize)]
struct Pair {
    secret: String,
    name: Option<String>,
}

async fn pair(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Ok(payload) = serde_json::from_slice::<Pair>(&body) else {
        return failure("device.secret_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    if payload.secret.is_empty()
        || payload.secret.len() > 200
        || payload.name.as_ref().is_some_and(|v| v.len() > 200)
    {
        return failure("device.secret_invalid", StatusCode::BAD_REQUEST, &headers);
    }
    let result = state
        .registry
        .lock()
        .expect("registry lock")
        .redeem(&payload.secret, payload.name.as_deref());
    match result {
        Ok(Some((device_id, token))) => {
            Json(json!({"device_id": device_id, "token": token, "node": state.node()}))
                .into_response()
        }
        Ok(None) => failure("device.secret_invalid", StatusCode::FORBIDDEN, &headers),
        Err(_) => failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers),
    }
}

async fn devices(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    device: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(Extension(AuthenticatedDevice(current))) = device else {
        return failure("device.unpaired", StatusCode::UNAUTHORIZED, &headers);
    };
    Json(
        state
            .registry
            .lock()
            .expect("registry lock")
            .listing(&current),
    )
    .into_response()
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    device: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    if device.is_none() {
        return failure("device.unpaired", StatusCode::UNAUTHORIZED, &headers);
    }
    match state.registry.lock().expect("registry lock").revoke(&id) {
        Ok(true) => {
            state.close_calls(&id);
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => failure("device.not_found", StatusCode::NOT_FOUND, &headers),
        Err(_) => failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers),
    }
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(
        json!({"launch_id": state.launch_id, "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"), "api": API, "fingerprint": state.identity.fingerprint,
        "public_key": state.identity.public_key, "host": state.host, "calls": state.open_calls()}),
    )
}

#[derive(Deserialize, Default)]
struct LocalPair {
    name: Option<String>,
}

async fn pair_local(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let payload = if body.is_empty() {
        LocalPair::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return failure("device.secret_invalid", StatusCode::BAD_REQUEST, &headers),
        }
    };
    if payload.name.as_ref().is_some_and(|v| v.len() > 200) {
        return failure("device.secret_invalid", StatusCode::BAD_REQUEST, &headers);
    }
    let result = state
        .registry
        .lock()
        .expect("registry lock")
        .pair_local(payload.name.as_deref());
    match result {
        Ok((device_id, token, replaced)) => {
            for id in replaced {
                state.close_calls(&id);
            }
            Json(json!({"device_id": device_id, "token": token, "node": state.node()}))
                .into_response()
        }
        Err(_) => failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers),
    }
}

async fn unpair_local(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match state.registry.lock().expect("registry lock").revoke_local() {
        Ok(revoked) => {
            for id in &revoked {
                state.close_calls(id);
            }
            Json(json!({"ok": true, "revoked": !revoked.is_empty()})).into_response()
        }
        Err(_) => failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers),
    }
}

async fn call_socket(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let protocols: Vec<_> = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .collect();
    let token = protocols
        .iter()
        .find_map(|value| value.strip_prefix("sidevoice.token."));
    let device = token.and_then(|value| {
        state
            .registry
            .lock()
            .expect("registry lock")
            .authenticate(value)
    });
    let language = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .unwrap_or("en");
    let close_reason = render(&LocalizedMessage::new("device.unpaired"), language);
    let ws = if protocols.contains(&"sidevoice") {
        ws.protocols(["sidevoice"])
    } else {
        ws
    };
    ws.on_upgrade(
        move |socket| async move { socket_loop(state, device, socket, close_reason).await },
    )
    .into_response()
}

async fn socket_loop(
    state: Arc<AppState>,
    device: Option<String>,
    mut socket: WebSocket,
    close_reason: String,
) {
    let Some(id) = device else {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 4401,
                reason: close_reason.into(),
            })))
            .await;
        return;
    };
    let (sender, mut revoked) = watch::channel(false);
    state
        .calls
        .lock()
        .expect("calls lock")
        .entry(id.clone())
        .or_default()
        .push(sender);
    let (events, mut output) = tokio::sync::mpsc::channel::<Value>(128);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let hello = loop {
        tokio::select! {
            _ = revoked.changed() => {
                let _ = socket.send(Message::Close(Some(CloseFrame { code: 4401, reason: close_reason.clone().into() }))).await;
                break None;
            }
            input = tokio::time::timeout_at(deadline, socket.recv()) => match input {
                Ok(Some(Ok(Message::Text(text)))) => break Some(serde_json::from_str::<Value>(&text).unwrap_or_default()),
                Ok(Some(Ok(Message::Ping(bytes)))) => { let _ = socket.send(Message::Pong(bytes)).await; },
                Ok(Some(Ok(Message::Pong(_)))) => {},
                _ => break None,
            }
        }
    };
    let Some(hello) = hello else {
        unregister_call(&state, &id);
        return;
    };
    let defaults = crate::models::default_settings(Some(&crate::runtime::system_language()), None);
    let loaded =
        crate::models::settings_from(hello.get("data").and_then(|v| v.get("settings")), &defaults);
    if let Some(refusal) = crate::models::unavailable(&loaded.settings, |_| false) {
        let _ = socket.send(Message::Text(json!({"type":"error","data":crate::messages::render_refusal(&refusal,&loaded.settings.ui_language)}).to_string().into())).await;
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 1008,
                reason: "".into(),
            })))
            .await;
        unregister_call(&state, &id);
        return;
    }
    let session = match state
        .room
        .join(id.clone(), loaded.settings.ui_language.clone(), events)
    {
        Ok(session) => session,
        Err(_) => {
            let admission = state.room.admission(&loaded.settings.ui_language);
            let _=socket.send(Message::Text(json!({"type":"error","data":{"message":admission["message"],"reason":admission["reason"]}}).to_string().into())).await;
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 1013,
                    reason: "".into(),
                })))
                .await;
            unregister_call(&state, &id);
            return;
        }
    };
    let room_info = json!({"api": API, "version": env!("CARGO_PKG_VERSION")});
    let _ = socket.send(Message::Text(json!({"type":"voice-session","data":{"session_id":session,"sample_rate":16000,"channels":1,"room":room_info}}).to_string().into())).await;
    loop {
        tokio::select! {
            _ = revoked.changed() => {
                let _ = socket.send(Message::Close(Some(CloseFrame { code: 4401, reason: close_reason.into() }))).await;
                break;
            }
            event = output.recv() => if let Some(event) = event {
                if socket.send(Message::Text(event.to_string().into())).await.is_err() { break; }
            } else { break; },
            message = socket.recv() => match message {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Ping(bytes))) => {let _=socket.send(Message::Pong(bytes)).await;},
                Some(Ok(Message::Text(raw))) if raw.len()<=1024*1024 => {
                    if let Ok(value)=serde_json::from_str::<Value>(&raw){
                        let data=value.get("data").filter(|v|v.is_object()).cloned().unwrap_or_default();
                        if data.get("session_id").and_then(Value::as_str)==Some(session.as_str()){
                            match value.get("type").and_then(Value::as_str){
                                Some("voice-client-error")=>{state.room.report_client_error(&data);},
                                Some("voice-settings")=>{
                                    let loaded=crate::models::settings_from(data.get("settings"),&defaults);
                                    if let Some(issue)=loaded.issue{let _=socket.send(Message::Text(json!({"type":"error","data":{"message":issue}}).to_string().into())).await;}
                                    else if let Some(refusal)=crate::models::unavailable(&loaded.settings,|_|false){let _=socket.send(Message::Text(json!({"type":"error","data":crate::messages::render_refusal(&refusal,&loaded.settings.ui_language)}).to_string().into())).await;}
                                    else{state.room.set_language(&session,&loaded.settings.ui_language);}
                                },
                                _=>{}
                            }
                        }
                    }
                },
                _=>{},
            },
        }
    }
    state.room.leave(&session);
    drop(revoked);
    unregister_call(&state, &id);
}

fn unregister_call(state: &AppState, id: &str) {
    let mut calls = state.calls.lock().expect("calls lock");
    if let Some(senders) = calls.get_mut(id) {
        senders.retain(|sender| sender.receiver_count() > 0);
        if senders.is_empty() {
            calls.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn one_time_code_redeems_over_real_router() {
        let temp = tempfile::tempdir().unwrap();
        let dir = PrivateDir::open(temp.path().join("core")).unwrap();
        let identity = NodeIdentity::load_or_create(&dir).unwrap();
        let registry = DeviceRegistry::load(dir.clone()).unwrap();
        let state = Arc::new(AppState::new(
            dir.clone(),
            identity,
            registry,
            "fixture".to_owned(),
            "fixture-host".to_owned(),
            8768,
            Arc::new(Room::load(dir.clone()).unwrap()),
        ));
        let code = state.issue_code();
        assert!(code["code"].as_str().unwrap().starts_with("SV1."));
        let secret = code["payload"]["secret"].as_str().unwrap();
        let body = serde_json::to_vec(&json!({"secret": secret, "name": "Test device"})).unwrap();
        let app = router(state, false);
        let pair = || {
            Request::builder()
                .method("POST")
                .uri("/api/device/pair")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap()
        };
        let first = app.clone().oneshot(pair()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let response: Value =
            serde_json::from_slice(&to_bytes(first.into_body(), 4096).await.unwrap()).unwrap();
        let token = response["token"].as_str().unwrap();
        let list = Request::builder()
            .uri("/api/device/devices")
            .header("host", "localhost")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(list).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            app.oneshot(pair()).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            dir.read_json("devices.json").unwrap().unwrap()["devices"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
    }
}
