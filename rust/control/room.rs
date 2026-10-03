//! One owner for connector credentials, bindings and process-local conversation state.
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use rand::RngCore;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use crate::storage::PrivateDir;

const MAX_HISTORY: usize = 2000;
const MAX_UTTERANCES: usize = 2048;
const MAX_BROWSERS: usize = 8;
const INPUT_TTL: u64 = 600;
const ACK_TIMEOUT: Duration = Duration::from_secs(60);

fn seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn field<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}
fn valid_thread(thread: &str) -> bool {
    !thread.is_empty()
        && thread.len() <= 200
        && thread
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
fn valid_delivery_ack(value: &Value) -> Option<&str> {
    let fields = value.as_object()?;
    if serde_json::to_vec(value).ok()?.len() > 4096
        || fields
            .keys()
            .any(|k| !["status", "detail", "error"].contains(&k.as_str()))
    {
        return None;
    }
    for key in ["detail", "error"] {
        if fields
            .get(key)
            .is_some_and(|v| v.as_str().is_none_or(|s| s.len() > 1000))
        {
            return None;
        }
    }
    let status = field(value, "status");
    [
        "accepted",
        "unknown",
        "unsupported",
        "failed",
        "unknown_binding",
    ]
    .contains(&status)
    .then_some(status)
}

#[derive(Clone, Debug)]
pub struct RoomError {
    pub status: u16,
    pub key: &'static str,
}
impl RoomError {
    fn new(status: u16, key: &'static str) -> Self {
        Self { status, key }
    }
}

#[derive(Debug)]
pub struct PeerError;
pub struct PeerRequest {
    pub method: String,
    pub params: Value,
    pub answer: Option<oneshot::Sender<Result<Value, PeerError>>>,
}
#[derive(Clone)]
pub struct ConnectorPeer {
    pub generation: String,
    pub sender: mpsc::Sender<PeerRequest>,
    pub stop: watch::Sender<bool>,
}
impl ConnectorPeer {
    pub fn disconnect(&self) {
        let _ = self.stop.send(true);
    }
    pub async fn send(&self, method: &str, params: Value) -> Result<(), PeerError> {
        self.sender
            .send(PeerRequest {
                method: method.into(),
                params,
                answer: None,
            })
            .await
            .map_err(|_| PeerError)
    }
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, PeerError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(PeerRequest {
                method: method.into(),
                params,
                answer: Some(tx),
            })
            .await
            .map_err(|_| PeerError)?;
        tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| PeerError)?
            .map_err(|_| PeerError)?
    }
}

struct Browser {
    device: String,
    language: String,
    sender: mpsc::Sender<Value>,
    target: Option<Target>,
    revision: u64,
    turn_revision: u64,
    speaking: bool,
    sent: u64,
    active: Option<String>,
    pending: VecDeque<String>,
}
#[derive(Clone)]
struct Target {
    thread: String,
    title: Option<String>,
    binding_id: String,
}
impl Target {
    fn view(&self) -> Value {
        json!({"thread_id": self.thread, "title": self.title, "binding_id": self.binding_id})
    }
}
/// Focus and revision captured atomically when a browser opens a voice turn.
#[derive(Clone, Debug)]
pub struct VoiceTurn {
    pub session_id: String,
    pub revision: u64,
    pub thread_id: Option<String>,
    pub binding_id: Option<String>,
    pub title: Option<String>,
    language: String,
}
struct Binding {
    id: String,
    connector: String,
    thread: String,
    harness: String,
    title: Option<String>,
    created: u64,
    active: bool,
    live: bool,
    inbound: Option<Value>,
    capabilities: Value,
    engine: Option<Value>,
    route: Option<String>,
}
impl Binding {
    fn view(&self) -> Value {
        json!({"id": self.id, "connector": self.connector, "thread": self.thread,
        "harness": self.harness, "title": self.title, "created": self.created, "active": self.active as u8,
        "inbound": self.inbound, "capabilities": self.capabilities, "engine": self.engine, "route": self.route,
        "connected": self.live})
    }
}
struct Row {
    seq: u64,
    id: String,
    thread: String,
    role: &'static str,
    text: String,
    name: Option<String>,
    session: String,
    revision: u64,
    time: u64,
    status: String,
    reason: Option<String>,
    language: Option<String>,
    offline: Option<Value>,
    payload: Option<Value>,
    queued_at: u64,
    attempts: usize,
    next_attempt: u64,
}
struct InputDraft<'a> {
    row_id: String,
    text: &'a str,
    session_id: &'a str,
    revision: u64,
    thread_id: &'a str,
    binding_id: &'a str,
    title: Option<String>,
    language: &'a str,
    message_id: &'a str,
}
impl Row {
    fn view(&self) -> Value {
        json!({"seq": self.seq, "id": self.id, "thread": self.thread, "role": self.role,
        "text": self.text, "name": self.name, "session": self.session, "revision": self.revision,
        "time": self.time, "status": self.status, "audio_reason": self.reason, "offline": self.offline})
    }
}
struct UtteranceRecord {
    row_id: String,
    clients: HashMap<String, (u64, String)>,
    parked: bool,
}
struct Inner {
    connectors: Map<String, Value>,
    pairing: HashMap<String, u64>,
    peers: HashMap<String, ConnectorPeer>,
    peer_order: VecDeque<String>,
    bindings: HashMap<String, Binding>,
    browsers: HashMap<String, Browser>,
    sessions: VecDeque<String>,
    rows: VecDeque<Row>,
    utterances: HashMap<String, UtteranceRecord>,
    client_errors: VecDeque<Value>,
    seq: u64,
    working: HashMap<String, bool>,
    inflight: HashMap<String, String>,
}

pub struct Room {
    dir: PrivateDir,
    inner: Mutex<Inner>,
}
impl Room {
    pub fn load(dir: PrivateDir) -> io::Result<Self> {
        let state = match dir.read_json("room-state.json")? {
            Some(v) => v,
            None => dir.import_legacy_connectors()?,
        };
        let connectors = state
            .get("connectors")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            dir,
            inner: Mutex::new(Inner {
                connectors,
                pairing: HashMap::new(),
                peers: HashMap::new(),
                peer_order: VecDeque::new(),
                bindings: HashMap::new(),
                browsers: HashMap::new(),
                sessions: VecDeque::new(),
                rows: VecDeque::new(),
                utterances: HashMap::new(),
                client_errors: VecDeque::new(),
                seq: 0,
                working: HashMap::new(),
                inflight: HashMap::new(),
            }),
        })
    }
    fn save(&self, inner: &Inner) -> io::Result<()> {
        self.dir
            .write_json("room-state.json", &json!({"connectors": inner.connectors}))
    }
    pub fn local_credential(&self) -> io::Result<(String, String)> {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(saved) = self.dir.read_json("connector-credential.json").ok().flatten() {
            let cid = field(&saved, "connector_id");
            let token = field(&saved, "token");
            if self.credential_locked(&inner, cid, token) == "paired" {
                return Ok((cid.into(), token.into()));
            }
        }
        let cid = id();
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let at = seconds();
        inner.connectors.insert(
            cid.clone(),
            json!({"token_hash": hash(&token), "created": at, "last_seen": at, "revoked": 0}),
        );
        self.save(&inner)?;
        self.dir.write_json(
            "connector-credential.json",
            &json!({"connector_id": cid, "token": token}),
        )?;
        Ok((cid, token))
    }
    fn credential_locked(&self, inner: &Inner, cid: &str, token: &str) -> &'static str {
        if cid.is_empty() || token.is_empty() {
            return "unknown";
        }
        let Some(entry) = inner.connectors.get(cid) else {
            return "unknown";
        };
        let expected = field(entry, "token_hash");
        let supplied = hash(token);
        if expected.len() != supplied.len()
            || expected
                .as_bytes()
                .iter()
                .zip(supplied.as_bytes())
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                != 0
        {
            return "unknown";
        }
        if entry
            .get("revoked")
            .is_some_and(|v| v == true || v.as_i64().unwrap_or_default() != 0)
        {
            "revoked"
        } else {
            "paired"
        }
    }
    pub fn authenticate_connector(&self, cid: &str, token: &str, identity: &Value) -> bool {
        let mut inner = self.inner.lock().expect("room lock");
        if self.credential_locked(&inner, cid, token) != "paired" {
            return false;
        }
        if let Some(entry) = inner.connectors.get_mut(cid).and_then(Value::as_object_mut) {
            entry.insert("last_seen".into(), json!(seconds()));
            for (key, limit) in [("host", 200), ("platform", 60), ("version", 40)] {
                if let Some(s) = identity
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    entry.insert(
                        key.into(),
                        json!(s.trim().chars().take(limit).collect::<String>()),
                    );
                }
            }
            if let Some(names) = identity.get("harnesses").and_then(Value::as_array) {
                entry.insert(
                    "harnesses".into(),
                    json!(names
                        .iter()
                        .take(8)
                        .filter_map(Value::as_str)
                        .map(|s| s.trim().chars().take(40).collect::<String>())
                        .collect::<Vec<_>>()),
                );
            }
        }
        self.save(&inner).is_ok()
    }
    pub fn pairing_code(&self) -> String {
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut bytes = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let code: String = bytes
            .iter()
            .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
            .collect();
        let mut inner = self.inner.lock().expect("room lock");
        inner.pairing.retain(|_, expiry| *expiry >= seconds());
        inner.pairing.insert(code.clone(), seconds() + 180);
        format!("{}-{}-{}", &code[..4], &code[4..8], &code[8..])
    }
    pub fn redeem_pairing(
        &self,
        code: &str,
        identity: &Value,
    ) -> io::Result<Option<(String, String)>> {
        let normalized: String = code
            .chars()
            .filter(|c| !" -_.".contains(*c))
            .map(|c| match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            })
            .collect();
        let mut inner = self.inner.lock().expect("room lock");
        if inner
            .pairing
            .remove(&normalized)
            .is_none_or(|expiry| expiry < seconds())
        {
            return Ok(None);
        }
        let cid = id();
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut record = json!({"token_hash": hash(&token), "created": seconds(), "last_seen": seconds(), "revoked": 0});
        if let Some(obj) = record.as_object_mut() {
            for key in ["host", "platform", "version", "harnesses"] {
                if let Some(value) = identity.get(key) {
                    obj.insert(key.into(), value.clone());
                }
            }
        }
        inner.connectors.insert(cid.clone(), record);
        self.save(&inner)?;
        Ok(Some((cid, token)))
    }
    pub fn binding_views(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        json!(inner
            .bindings
            .values()
            .filter(|b| b.active)
            .map(Binding::view)
            .collect::<Vec<_>>())
    }
    pub fn paired_connectors(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let mut entries: Vec<Value> = inner
            .connectors
            .iter()
            .map(|(id, row)| {
                let mut result = row.as_object().cloned().unwrap_or_default();
                result.remove("token_hash");
                result.insert("id".into(), json!(id));
                result.insert("connected".into(), json!(inner.peers.contains_key(id)));
                Value::Object(result)
            })
            .collect();
        entries.sort_by_key(|e| e.get("created").and_then(Value::as_u64).unwrap_or_default());
        json!(entries)
    }
    pub fn attach(&self, cid: &str, peer: ConnectorPeer) -> Option<ConnectorPeer> {
        let mut inner = self.inner.lock().expect("room lock");
        let old = inner.peers.insert(cid.into(), peer);
        inner.peer_order.retain(|id| id != cid);
        inner.peer_order.push_back(cid.into());
        let ids: Vec<String> = inner
            .bindings
            .values_mut()
            .filter(|b| b.connector == cid && b.active)
            .map(|b| {
                b.live = false;
                b.id.clone()
            })
            .collect();
        for bid in ids {
            if let Some(row_id) = inner.inflight.remove(&bid) {
                if let Some(row) = inner.rows.iter_mut().find(|r| r.id == row_id) {
                    row.status = "pending".into();
                    row.next_attempt = 0;
                }
            }
        }
        old
    }
    pub fn detach(&self, cid: &str, generation: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        if inner
            .peers
            .get(cid)
            .is_none_or(|p| p.generation != generation)
        {
            return;
        }
        inner.peers.remove(cid);
        inner.peer_order.retain(|id| id != cid);
        let ids: Vec<String> = inner
            .bindings
            .values_mut()
            .filter(|b| b.connector == cid)
            .map(|b| {
                b.live = false;
                b.id.clone()
            })
            .collect();
        for bid in ids {
            if let Some(row_id) = inner.inflight.remove(&bid) {
                if let Some(row) = inner.rows.iter_mut().find(|r| r.id == row_id) {
                    row.status = "pending".into();
                    row.next_attempt = 0;
                }
            }
        }
    }
    pub fn current_peer(&self, cid: &str, generation: &str) -> bool {
        self.inner
            .lock()
            .expect("room lock")
            .peers
            .get(cid)
            .is_some_and(|peer| peer.generation == generation)
    }
    pub fn connector_peer(&self) -> Option<ConnectorPeer> {
        let inner = self.inner.lock().expect("room lock");
        inner
            .peer_order
            .iter()
            .rev()
            .find_map(|cid| inner.peers.get(cid).cloned())
    }
    pub fn register(&self, cid: &str, data: &Value) -> Result<Value, RoomError> {
        let thread = field(data, "thread");
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.peers.contains_key(cid) {
            return Err(RoomError::new(409, "room.connector_disconnected"));
        }
        let requested = field(data, "binding_id");
        if let Some(existing) = inner.bindings.get(requested) {
            if existing.connector != cid {
                return Err(RoomError::new(409, "room.binding_foreign"));
            }
        }
        let existing = if !requested.is_empty() && inner.bindings.contains_key(requested) {
            Some(requested.to_owned())
        } else {
            inner
                .bindings
                .values()
                .filter(|b| b.connector == cid && b.thread == thread && b.active)
                .max_by_key(|b| b.created)
                .map(|b| b.id.clone())
        };
        let bid = existing.unwrap_or_else(id);
        let capabilities = capabilities(data.get("capabilities"), data.get("experimental"));
        let engine = engine(data.get("engine"));
        let route = match field(data, "route") {
            "cursor-editor-bridge" | "cursor-editor-view" | "cursor-cli-persist" | "cursor-cli" => {
                Some(field(data, "route").into())
            }
            _ => None,
        };
        let title = data
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(200).collect());
        let harness = data
            .get("harness")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .chars()
            .take(40)
            .collect();
        let inbound = data.get("inbound").filter(|v| v.is_object()).cloned();
        let binding = inner
            .bindings
            .entry(bid.clone())
            .or_insert_with(|| Binding {
                id: bid.clone(),
                connector: cid.into(),
                thread: thread.into(),
                harness: String::new(),
                title: None,
                created: seconds(),
                active: true,
                live: true,
                inbound: None,
                capabilities: Value::Null,
                engine: None,
                route: None,
            });
        binding.harness = harness;
        binding.active = true;
        binding.live = true;
        if title.is_some() {
            binding.title = title;
        }
        if inbound.is_some() {
            binding.inbound = inbound;
        }
        binding.capabilities = capabilities;
        if engine.is_some() {
            binding.engine = engine;
        }
        binding.route = route;
        let actual_thread = binding.thread.clone();
        inner.working.remove(&actual_thread);
        Ok(
            json!({"client_ref": data.get("client_ref"), "binding_id": bid, "thread": actual_thread}),
        )
    }
    pub fn unregister(&self, cid: &str, bid: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(b) = inner.bindings.get_mut(bid).filter(|b| b.connector == cid) {
            b.active = false;
            b.live = false;
            let thread = b.thread.clone();
            inner.working.remove(&thread);
        }
    }
    pub fn close_channel(
        &self,
        thread: &str,
    ) -> Result<(Value, Option<(ConnectorPeer, Value)>), RoomError> {
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let binding_id = inner
            .bindings
            .values()
            .filter(|b| b.thread == thread && b.active)
            .max_by_key(|b| b.created)
            .map(|b| b.id.clone());
        if binding_id.is_none() && !inner.rows.iter().any(|r| r.thread == thread) {
            return Err(RoomError::new(409, "room.conversation_missing"));
        }
        let notify = binding_id
            .as_ref()
            .and_then(|bid| inner.bindings.get(bid))
            .and_then(|b| {
                inner.peers.get(&b.connector).cloned().map(|p| {
                    (
                        p,
                        json!({"binding_id":b.id,"thread":thread,"reason":"closed_from_room"}),
                    )
                })
            });
        if let Some(bid) = binding_id.as_ref() {
            if let Some(b) = inner.bindings.get_mut(bid) {
                b.active = false;
                b.live = false;
            }
            inner.inflight.remove(bid);
        }
        inner.working.remove(thread);
        let mut cancelled = Vec::new();
        for row in inner.rows.iter_mut().filter(|r| {
            r.role == "user"
                && r.thread == thread
                && matches!(r.status.as_str(), "pending" | "sending")
        }) {
            row.status = "not_sent".into();
            row.reason = Some("channel_closed".into());
            cancelled.push((
                row.id.clone(),
                row.session.clone(),
                row.payload.clone().unwrap_or_default(),
            ));
        }
        for (row_id, sid, payload) in cancelled {
            if let Some(browser) = inner.browsers.get(&sid) {
                let _ = browser
                    .sender
                    .try_send(json!({"type":"voice-input-receipt","data":{
                    "revision":payload["revision"],"history_id":row_id,"thread_id":thread,
                    "session_id":sid,"status":"not_sent"}}));
            }
        }
        let affected: Vec<String> = inner
            .browsers
            .iter()
            .filter(|(_, b)| b.target.as_ref().is_some_and(|t| t.thread == thread))
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in affected {
            if let Some(browser) = inner.browsers.get_mut(&sid) {
                browser.revision += 1;
                browser.target = Some(Target {
                    thread: String::new(),
                    title: None,
                    binding_id: id(),
                });
                browser.speaking = false;
                let _=browser.sender.try_send(json!({"type":"voice-cancel","data":{"session_id":sid,"revision":browser.revision}}));
            }
            interrupt_client(&mut inner, &sid, "focus_changed");
        }
        Ok((json!({"status":"closed","binding_id":binding_id}), notify))
    }
    pub fn participants(&self, session: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let current = session.and_then(|sid| inner.browsers.get(sid));
        let selected = current
            .and_then(|b| b.target.as_ref())
            .map(|t| t.thread.as_str());
        let language = current.map_or("en", |b| b.language.as_str());
        json!(inner.bindings.values().filter(|b| b.active).map(|b| {
            let host=inner.connectors.get(&b.connector).and_then(|c| c.get("host"));
            json!({"thread_id":b.thread,"title":b.title.clone().unwrap_or_else(||crate::messages::render(&crate::messages::LocalizedMessage::new("room.conversation_title").with_param("id",b.thread.chars().take(8).collect::<String>()),language)),
                "harness":b.harness,"available":b.live,"machine":{"id":b.connector,"host":host},
                "capabilities":b.capabilities,"engine":b.engine,"route":b.route,
                "reach":reachability(b,language),"selected":selected==Some(b.thread.as_str())})
        }).collect::<Vec<_>>())
    }
    pub fn join(
        &self,
        device: String,
        language: String,
        sender: mpsc::Sender<Value>,
    ) -> Result<String, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let max = std::env::var("VOICE_MAX_BROWSERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(MAX_BROWSERS)
            .max(1);
        if inner.browsers.len() >= max {
            return Err(RoomError::new(429, "room.full"));
        }
        let sid = id();
        inner.sessions.push_back(sid.clone());
        if inner.sessions.len() > 64 {
            inner.sessions.pop_front();
        }
        inner.browsers.insert(
            sid.clone(),
            Browser {
                device,
                language,
                sender,
                target: None,
                revision: 0,
                turn_revision: 0,
                speaking: false,
                sent: 0,
                active: None,
                pending: VecDeque::new(),
            },
        );
        Ok(sid)
    }
    pub fn leave(&self, sid: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        inner.browsers.remove(sid);
        interrupt_client(&mut inner, sid, "call_ended");
    }
    pub fn set_language(&self, sid: &str, language: &str) {
        if let Some(browser) = self.inner.lock().expect("room lock").browsers.get_mut(sid) {
            browser.language = language.to_owned();
        }
    }
    pub fn report_client_error(&self, report: &Value) -> Value {
        let mut inner = self.inner.lock().expect("room lock");
        let clipped =
            |key: &str, max: usize| field(report, key).chars().take(max).collect::<String>();
        inner.client_errors.push_back(json!({"session_id":clipped("session_id",100),"kind":clipped("kind",40),"message":clipped("message",200),"at":millis()}));
        while inner.client_errors.len() > 20 {
            inner.client_errors.pop_front();
        }
        json!({"status":"recorded"})
    }
    pub fn admission(&self, language: &str) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let max = std::env::var("VOICE_MAX_BROWSERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(MAX_BROWSERS)
            .max(1);
        json!({"admitted":inner.browsers.len()<max,"reason":if inner.browsers.len()>=max {Some("room_is_full")} else {None},
        "message":if inner.browsers.len()>=max {Some(crate::messages::render(&crate::messages::LocalizedMessage::new("room.full"),language))} else {None},"clients":inner.browsers.len(),"max":max})
    }
    pub fn select(&self, sid: &str, thread: &str) -> Result<Value, RoomError> {
        if !valid_thread(thread) {
            return Err(RoomError::new(400, "room.thread_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let Some(binding) = inner
            .bindings
            .values()
            .filter(|b| b.active && b.thread == thread)
            .max_by_key(|b| b.created)
        else {
            return Err(RoomError::new(409, "room.conversation_disconnected"));
        };
        let title = binding.title.clone();
        let Some(client) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if client.target.as_ref().is_some_and(|t| t.thread == thread) {
            return Ok(
                json!({"status":"already_active","binding":client.target.as_ref().map(Target::view)}),
            );
        }
        client.revision += 1;
        client.active = None;
        client.speaking = false;
        client.sent = 0;
        let _ = client.sender.try_send(
            json!({"type":"voice-cancel","data":{"session_id":sid,"revision":client.revision}}),
        );
        let target = Target {
            thread: thread.into(),
            title,
            binding_id: id(),
        };
        let view = target.view();
        client.target = Some(target);
        interrupt_client(&mut inner, sid, "focus_changed");
        Ok(json!({"status":"activated","binding":view}))
    }
    pub fn deselect(&self, sid: &str, bid: &str) -> Result<Value, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(c) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        if c.target.as_ref().is_none_or(|t| t.binding_id != bid) {
            return Err(RoomError::new(409, "room.focus_changed"));
        }
        c.revision += 1;
        c.active = None;
        c.speaking = false;
        let _ = c.sender.try_send(
            json!({"type":"voice-cancel","data":{"session_id":sid,"revision":c.revision}}),
        );
        let target = Target {
            thread: String::new(),
            title: None,
            binding_id: id(),
        };
        let view = target.view();
        c.target = Some(target);
        interrupt_client(&mut inner, sid, "focus_changed");
        Ok(json!({"status":"activated","binding":view}))
    }
    pub fn begin_turn(&self, sid: &str) -> Result<VoiceTurn, RoomError> {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(c) = inner.browsers.get_mut(sid) else {
            return Err(RoomError::new(409, "room.browser_absent"));
        };
        c.revision += 1;
        c.turn_revision = c.revision;
        c.speaking = true;
        let _ = c.sender.try_send(
            json!({"type":"voice-cancel","data":{"session_id":sid,"revision":c.revision}}),
        );
        let result = VoiceTurn {
            session_id: sid.into(),
            revision: c.revision,
            thread_id: c
                .target
                .as_ref()
                .filter(|t| !t.thread.is_empty())
                .map(|t| t.thread.clone()),
            binding_id: c
                .target
                .as_ref()
                .filter(|t| !t.thread.is_empty())
                .map(|t| t.binding_id.clone()),
            title: c.target.as_ref().and_then(|t| t.title.clone()),
            language: c.language.clone(),
        };
        hold_client(&mut inner, sid, result.revision);
        Ok(result)
    }
    pub fn finish_turn(&self, sid: &str, revision: u64) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(c) = inner.browsers.get_mut(sid) {
            if c.turn_revision == revision {
                c.speaking = false;
            }
        }
        let waiting: Vec<(String, String)> = inner
            .utterances
            .iter()
            .filter(|(_, u)| {
                u.clients
                    .get(sid)
                    .is_some_and(|(rev, status)| *rev == revision && status == "waiting_for_turn")
            })
            .map(|(uid, u)| (uid.clone(), u.row_id.clone()))
            .collect();
        for (uid, row_id) in waiting {
            let Some(row) = inner.rows.iter().find(|r| r.id == row_id) else {
                continue;
            };
            let thread = row.thread.clone();
            let Some(browser) = inner.browsers.get(sid) else {
                continue;
            };
            if browser.speaking
                || browser.revision != revision
                || browser.target.as_ref().is_none_or(|t| t.thread != thread)
            {
                continue;
            }
            if let Some(record) = inner.utterances.get_mut(&uid) {
                if let Some(entry) = record.clients.get_mut(sid) {
                    entry.1 = "queued".into();
                }
            }
            sync_row(&mut inner, &row_id, "queued", None);
        }
        dispatch_client(&mut inner, sid);
    }
    pub fn send_text(
        &self,
        text: &str,
        sid: &str,
        thread: &str,
        bid: &str,
        message_id: &str,
    ) -> Result<Value, RoomError> {
        let row_id = format!("{sid}:user-text:{message_id}");
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(row) = inner.rows.iter().find(|r| r.id == row_id) {
            return if row.text == text && row.thread == thread {
                Ok(json!({"accepted":true,"id":row_id,"revision":row.revision}))
            } else {
                Err(RoomError::new(409, "room.message_conflict"))
            };
        }
        let Some(c) = inner.browsers.get(sid) else {
            return Err(RoomError::new(409, "room.focus_changed"));
        };
        if c.target
            .as_ref()
            .is_none_or(|t| t.thread != thread || t.binding_id != bid)
        {
            return Err(RoomError::new(409, "room.focus_changed"));
        }
        if text.trim().is_empty() {
            return Err(RoomError::new(422, "room.text_empty"));
        }
        let revision = c.revision;
        let language = c.language.clone();
        let title = c.target.as_ref().and_then(|t| t.title.clone());
        Ok(queue_input_locked(
            &mut inner,
            InputDraft {
                row_id,
                text,
                session_id: sid,
                revision,
                thread_id: thread,
                binding_id: bid,
                title,
                language: &language,
                message_id,
            },
        ))
    }
    /// Commit a completed transcript to the same memory journal/outbox as typed input.
    /// The captured focus may differ from today's selection after a mid-turn switch.
    pub fn queue_voice_input(&self, turn: &VoiceTurn, text: &str) -> Result<Value, RoomError> {
        if text.trim().is_empty() {
            return Err(RoomError::new(422, "room.text_empty"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.sessions.iter().any(|sid| sid == &turn.session_id) {
            return Err(RoomError::new(409, "room.focus_changed"));
        }
        let row_id = format!("{}:user-turn:{}", turn.session_id, turn.revision);
        if let Some(row) = inner.rows.iter().find(|row| row.id == row_id) {
            return if row.text == text && turn.thread_id.as_deref() == Some(row.thread.as_str()) {
                Ok(json!({"accepted":true,"id":row_id,"revision":row.revision}))
            } else {
                Err(RoomError::new(409, "room.message_conflict"))
            };
        }
        let (Some(thread), Some(bid)) = (turn.thread_id.as_deref(), turn.binding_id.as_deref())
        else {
            if let Some(browser) = inner.browsers.get(&turn.session_id) {
                let _ = browser
                    .sender
                    .try_send(json!({"type":"voice-input-receipt","data":{
                    "revision":turn.revision,"history_id":row_id,"thread_id":Value::Null,
                    "session_id":turn.session_id,"status":"not_sent"}}));
            }
            return Ok(
                json!({"accepted":false,"id":row_id,"revision":turn.revision,"status":"not_sent"}),
            );
        };
        let message_id = id();
        Ok(queue_input_locked(
            &mut inner,
            InputDraft {
                row_id,
                text,
                session_id: &turn.session_id,
                revision: turn.revision,
                thread_id: thread,
                binding_id: bid,
                title: turn.title.clone(),
                language: &turn.language,
                message_id: &message_id,
            },
        ))
    }
    pub fn history(&self, thread: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("room lock");
        json!({"messages":inner.rows.iter().filter(|r|thread.is_none_or(|t|r.thread==t)).rev().take(1000).collect::<Vec<_>>().into_iter().rev().map(Row::view).collect::<Vec<_>>()})
    }
    pub fn reply_language(&self, row_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("room lock")
            .rows
            .iter()
            .find(|r| r.id == row_id)
            .and_then(|r| r.language.clone())
    }
    pub fn snapshot(&self, sid: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let c = sid.and_then(|s| inner.browsers.get(s));
        let mut utterances:Vec<(u64,Value,Option<Value>)>=inner.utterances.iter().filter_map(|(uid,record)|{
            let row=inner.rows.iter().find(|r|r.id==record.row_id)?;
            let clients:Map<String,Value>=record.clients.iter().map(|(id,(_,status))|(id.clone(),json!(status))).collect();
            let own=sid.and_then(|id|record.clients.get(id)).map(|(_,status)|json!({"utterance_id":uid,"revision":row.revision,"session_id":sid,"status":status}));
            Some((row.seq,json!({"utterance_id":uid,"revision":row.revision,"thread_id":row.thread,"status":row.status,"parked":record.parked,"replay_of":Value::Null,"clients":clients}),own))
        }).collect();
        utterances.sort_by_key(|entry| entry.0);
        let room_utterances: Vec<Value> = utterances.iter().map(|entry| entry.1.clone()).collect();
        let call_utterances: Vec<Value> =
            utterances.into_iter().filter_map(|entry| entry.2).collect();
        json!({"binding":c.and_then(|b|b.target.as_ref()).filter(|t|!t.thread.is_empty()).map(Target::view),
        "room":{"revision":c.map_or(0,|b|b.revision),"speaking":c.is_some_and(|b|b.speaking),"switching":false,"clients":inner.browsers.len(),"utterances":room_utterances,"audio_reports":[],"client_errors":inner.client_errors},
        "clients":inner.browsers.iter().map(|(id,b)|json!({"id":id,"device_id":b.device,"connected":true,"user_speaking":b.speaking,"turn_revision":b.turn_revision,"transport":"pcm","transcription":Value::Null})).collect::<Vec<_>>(),
        "call":c.map(|b|json!({"id":sid,"target":b.target.as_ref().map(Target::view).unwrap_or(json!({})),"connected":true,"user_speaking":b.speaking,"error":Value::Null,"sent":b.sent,"last_delivery":Value::Null,"revision":b.revision,"utterances":call_utterances,"mic":Value::Null,"mic_settings":Value::Null,"transcription":Value::Null,"audio_health":Value::Null,"speech_filter":{}}))})
    }
    pub fn publish(&self, p: &Value, v3: bool) -> Value {
        let original_sid = field(p, "session_id");
        let thread = field(p, "thread_id");
        let requested_uid = field(p, "utterance_id");
        let text = field(p, "text");
        let generated = id();
        let uid = if requested_uid.is_empty() && !v3 {
            generated.as_str()
        } else {
            requested_uid
        };
        let mut revision = p.get("revision").and_then(Value::as_u64).unwrap_or(0);
        if text.is_empty()
            || text.len() > 6000
            || uid.len() > 200
            || !valid_thread(thread)
            || p.get("revision").and_then(Value::as_u64).is_none()
            || p.get("language")
                .and_then(Value::as_str)
                .is_some_and(|language| !["es", "en", "fr", "it", "pt", "hi"].contains(&language))
        {
            return json!({"status":"rejected","error":"room.speech_invalid","terminal":v3,"reason_code":"application_refusal"});
        }
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(record) = inner.utterances.get(uid) {
            if let Some(row) = inner.rows.iter().find(|r| r.id == record.row_id) {
                return if row.text == text && row.thread == thread {
                    json!({"status":row.status,"text_saved":true,"utterance_id":uid})
                } else {
                    json!({"status":"rejected","error":"room.speech_invalid","terminal":v3,"reason_code":"application_refusal"})
                };
            }
        }
        let audience: Vec<String> = inner
            .browsers
            .iter()
            .filter(|(_, b)| b.target.as_ref().is_some_and(|t| t.thread == thread))
            .map(|(id, _)| id.clone())
            .collect();
        let mut sid = original_sid.to_owned();
        if !inner.browsers.contains_key(original_sid) {
            if let Some(replacement) = inner
                .sessions
                .iter()
                .rev()
                .find(|candidate| audience.contains(candidate))
            {
                sid = replacement.clone();
                revision = inner.browsers.get(&sid).map_or(revision, |b| b.revision);
            }
        }
        let row_id = format!("{sid}:voice:{uid}");
        if let Some(row) = inner.rows.iter().find(|r| r.id == row_id) {
            return if row.text == text && row.thread == thread {
                json!({"status":row.status,"text_saved":true,"utterance_id":uid})
            } else {
                json!({"status":"rejected","error":"room.speech_invalid","terminal":v3,"reason_code":"application_refusal"})
            };
        }
        let asker = inner.browsers.get(&sid);
        let known = inner.sessions.iter().any(|s| s == &sid);
        let reason = if !known {
            Some("session_changed")
        } else if asker.is_none() {
            Some("call_ended")
        } else if asker.is_some_and(|b| b.target.as_ref().is_none_or(|t| t.thread != thread)) {
            Some("focus_changed")
        } else if asker.is_some_and(|b| b.revision != revision) {
            Some(if asker.is_some_and(|b| b.turn_revision > revision) {
                "newer_turn"
            } else {
                "focus_changed"
            })
        } else if asker.is_some_and(|b| b.speaking) {
            Some("user_speaking")
        } else {
            None
        };
        let capacity = inner.utterances.len() >= MAX_UTTERANCES
            || audience.iter().any(|id| {
                inner
                    .browsers
                    .get(id)
                    .is_some_and(|b| b.pending.len() >= 16)
            });
        let can_speak = reason.is_none_or(|r| ["newer_turn", "user_speaking"].contains(&r))
            && !audience.is_empty()
            && !capacity;
        let waiting = can_speak
            && audience
                .iter()
                .any(|id| inner.browsers.get(id).is_some_and(|b| b.speaking));
        let status = if !can_speak {
            "text_only"
        } else if waiting {
            "waiting_for_turn"
        } else {
            "queued"
        };
        let spoken_revision =
            if reason.is_some_and(|r| ["newer_turn", "user_speaking"].contains(&r)) {
                asker.map_or(revision, |b| b.revision)
            } else {
                revision
            };
        let reason = if capacity { Some("queue_full") } else { reason };
        let name = inner
            .bindings
            .values()
            .filter(|b| b.thread == thread && b.active)
            .max_by_key(|b| b.created)
            .and_then(|b| b.title.clone())
            .or_else(|| {
                asker
                    .and_then(|b| b.target.as_ref())
                    .and_then(|t| t.title.clone())
            })
            .or_else(|| {
                Some(crate::messages::render(
                    &crate::messages::LocalizedMessage::new("room.conversation_title")
                        .with_param("id", thread.chars().take(8).collect::<String>()),
                    asker.map_or("en", |b| b.language.as_str()),
                ))
            });
        inner.seq += 1;
        let seq = inner.seq;
        inner.rows.push_back(Row {
            seq,
            id: row_id.clone(),
            thread: thread.into(),
            role: "assistant",
            text: text.into(),
            name,
            session: sid.clone(),
            revision,
            time: millis(),
            status: status.into(),
            reason: reason.map(str::to_owned),
            language: p.get("language").and_then(Value::as_str).map(str::to_owned),
            offline: None,
            payload: None,
            queued_at: seconds(),
            attempts: 0,
            next_attempt: 0,
        });
        trim_rows(&mut inner);
        if can_speak {
            let mut clients = HashMap::new();
            for listener in &audience {
                if let Some(c) = inner.browsers.get(listener) {
                    let client_status = if c.speaking {
                        "waiting_for_turn"
                    } else {
                        "queued"
                    };
                    clients.insert(listener.clone(), (c.revision, client_status.into()));
                }
            }
            inner.utterances.insert(
                uid.into(),
                UtteranceRecord {
                    row_id: row_id.clone(),
                    clients,
                    parked: false,
                },
            );
            for listener in audience {
                if let Some(c) = inner.browsers.get_mut(&listener) {
                    c.pending.push_back(uid.into());
                }
                dispatch_client(&mut inner, &listener);
            }
        }
        if status == "text_only"
            && matches!(
                reason,
                Some("session_changed" | "call_ended" | "focus_changed")
            )
            && inner.utterances.len() < MAX_UTTERANCES
        {
            inner.utterances.insert(
                uid.into(),
                UtteranceRecord {
                    row_id,
                    clients: HashMap::new(),
                    parked: true,
                },
            );
        }
        if status == "text_only" {
            json!({"status":"text_only","text_saved":true,"reason":reason.unwrap_or("call_ended")})
        } else {
            json!({"status":status,"utterance_id":uid,"session_id":sid,"revision":spoken_revision,"text_saved":true})
        }
    }
    pub fn connector_speech(&self, cid: &str, p: &Value, v3: bool) -> Value {
        let bid = field(p, "binding_id");
        let thread = {
            let inner = self.inner.lock().expect("room lock");
            inner
                .bindings
                .get(bid)
                .filter(|b| b.connector == cid && b.live)
                .map(|b| b.thread.clone())
        };
        let Some(thread) = thread else {
            return if v3 {
                json!({"status":"unknown_binding","event_id":p.get("event_id"),"utterance_id":p.get("utterance_id")})
            } else {
                json!({"status":"rejected","error":"room.binding_foreign","event_id":p.get("event_id")})
            };
        };
        let mut speech = p.clone();
        speech["thread_id"] = json!(thread);
        self.publish(&speech, v3)
    }
    pub fn receipt(
        &self,
        sid: &str,
        uid: &str,
        revision: u64,
        status: &str,
    ) -> Result<Value, RoomError> {
        if ![
            "playing",
            "failed",
            "playback_finished",
            "skipped",
            "cancelled_unplayed",
            "cancelled_playing",
        ]
        .contains(&status)
        {
            return Err(RoomError::new(400, "room.receipt_invalid"));
        }
        let mut inner = self.inner.lock().expect("room lock");
        let Some(browser) = inner.browsers.get(sid) else {
            return Err(RoomError::new(409, "room.stale_utterance"));
        };
        let special = matches!(
            status,
            "skipped" | "cancelled_unplayed" | "cancelled_playing"
        );
        if !special && browser.revision != revision {
            return Err(RoomError::new(409, "room.stale_utterance"));
        }
        let active = browser.active.as_deref() == Some(uid);
        if matches!(status, "playing" | "failed" | "playback_finished")
            && (!active || browser.speaking)
        {
            return Err(RoomError::new(409, "room.stale_utterance"));
        }
        let Some(record) = inner.utterances.get_mut(uid) else {
            return Err(RoomError::new(409, "room.stale_utterance"));
        };
        let Some(entry) = record.clients.get_mut(sid) else {
            return Err(RoomError::new(409, "room.stale_utterance"));
        };
        if (special && revision > entry.0) || (!special && entry.0 != revision) {
            return Err(RoomError::new(409, "room.stale_utterance"));
        }
        if matches!(
            entry.1.as_str(),
            "failed" | "playback_finished" | "interrupted"
        ) {
            if special {
                return Ok(json!({"status":status}));
            }
            return Err(RoomError::new(409, "room.stale_utterance"));
        }
        if status == "cancelled_unplayed" && revision < entry.0 {
            return Ok(json!({"status":status}));
        }
        let next = match status {
            "skipped" | "cancelled_playing" => "interrupted",
            "cancelled_unplayed" => "waiting_for_turn",
            other => other,
        };
        entry.1 = next.into();
        let row_id = record.row_id.clone();
        let best = record
            .clients
            .values()
            .map(|(_, status)| status.as_str())
            .max_by_key(|status| status_rank(status))
            .unwrap_or(next)
            .to_owned();
        if let Some(row) = inner.rows.iter_mut().find(|r| r.id == row_id) {
            row.status = best;
            row.reason = match status {
                "skipped" => Some("user_skipped".into()),
                "cancelled_playing" => Some("user_interrupted".into()),
                "cancelled_unplayed" => Some("user_speaking".into()),
                "failed" => Some("playback_failed".into()),
                _ => None,
            };
        }
        let browser = inner.browsers.get_mut(sid).expect("browser present");
        if status != "playing" {
            if active {
                browser.active = None;
            }
            if status == "cancelled_unplayed" && !browser.pending.iter().any(|item| item == uid) {
                browser.pending.push_front(uid.into());
            } else if status != "cancelled_unplayed" {
                browser.pending.retain(|item| item != uid);
            }
            dispatch_client(&mut inner, sid);
        }
        Ok(json!({"status":status}))
    }
    pub fn working(&self, cid: &str, data: &Value) {
        let bid = field(data, "binding_id");
        let mut inner = self.inner.lock().expect("room lock");
        let Some(b) = inner
            .bindings
            .get(bid)
            .filter(|b| b.connector == cid && b.live)
        else {
            return;
        };
        let thread = b.thread.clone();
        let Some(working) = data.get("working").and_then(Value::as_bool) else {
            return;
        };
        inner.working.insert(thread.clone(), working);
        for c in inner
            .browsers
            .values()
            .filter(|c| c.target.as_ref().is_some_and(|t| t.thread == thread))
        {
            let mut out = json!({"thread_id":thread,"working":working});
            if let Some(obj) = out.as_object_mut() {
                for key in ["turn_id", "turn_phase", "session_id", "revision"] {
                    if let Some(v) = data.get(key) {
                        obj.insert(key.into(), v.clone());
                    }
                }
            }
            let _ = c
                .sender
                .try_send(json!({"type":"voice-conversation","data":out}));
        }
    }
    pub fn engine(&self, cid: &str, data: &Value) {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(b) = inner
            .bindings
            .get_mut(field(data, "binding_id"))
            .filter(|b| b.connector == cid && b.live)
        {
            if let Some(e) = engine(data.get("engine")) {
                if e.get("model").is_some() {
                    b.engine = Some(e);
                }
            }
        }
    }
    pub fn read(&self, cid: &str, data: &Value) {
        let mut inner = self.inner.lock().expect("room lock");
        let Some(b) = inner
            .bindings
            .get(field(data, "binding_id"))
            .filter(|b| b.connector == cid && b.live)
        else {
            return;
        };
        let thread = b.thread.clone();
        let mid = field(data, "message_id");
        if let Some(row) = inner.rows.iter_mut().rev().find(|r| {
            r.thread == thread
                && r.role == "user"
                && r.payload
                    .as_ref()
                    .is_some_and(|p| field(p, "message_id") == mid)
                && !matches!(r.status.as_str(), "read" | "not_sent")
        }) {
            row.status = "read".into();
            let sid = row.session.clone();
            let payload = row.payload.clone().unwrap_or_default();
            if let Some(c) = inner.browsers.get(&sid) {
                let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":payload["history_id"],"thread_id":thread,"session_id":sid,"status":"read"}}));
            }
        }
    }
    pub fn pending_delivery(&self) -> Vec<(String, String, ConnectorPeer, Value)> {
        let mut inner = self.inner.lock().expect("room lock");
        let now = seconds();
        let mut work = Vec::new();
        for ix in 0..inner.rows.len() {
            let row = &inner.rows[ix];
            if row.role != "user" || row.status != "pending" {
                continue;
            }
            let rid = row.id.clone();
            let thread = row.thread.clone();
            let queued = row.queued_at;
            let next = row.next_attempt;
            if now.saturating_sub(queued) >= INPUT_TTL {
                inner.rows[ix].status = "not_sent".into();
                inner.rows[ix].reason = Some("expired".into());
                let payload = inner.rows[ix].payload.clone().unwrap_or_default();
                let sid = inner.rows[ix].session.clone();
                if let Some(c) = inner.browsers.get(&sid) {
                    let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":rid,"thread_id":thread,"session_id":sid,"status":"not_sent"}}));
                }
                continue;
            }
            if next > now {
                continue;
            }
            let Some(b) = inner
                .bindings
                .values()
                .filter(|b| {
                    b.active && b.live && b.thread == thread && !inner.inflight.contains_key(&b.id)
                })
                .max_by_key(|b| b.created)
            else {
                continue;
            };
            let bid = b.id.clone();
            let Some(peer) = inner.peers.get(&b.connector).cloned() else {
                continue;
            };
            let row = &inner.rows[ix];
            let payload = row.payload.as_ref().unwrap();
            let data = json!({"event_id":rid,"binding_id":bid,"thread":thread,"text":row.text,"channel":"voice","session_id":payload["session_id"],"revision":payload["revision"],"message_id":payload["message_id"]});
            inner.rows[ix].status = "sending".into();
            inner.inflight.insert(bid.clone(), rid.clone());
            work.push((bid, rid, peer, data));
        }
        work
    }
    pub fn settle_delivery(
        &self,
        bid: &str,
        rid: &str,
        generation: &str,
        answer: Result<Value, PeerError>,
    ) {
        let mut inner = self.inner.lock().expect("room lock");
        if inner.inflight.get(bid).is_none_or(|id| id != rid) {
            return;
        }
        let Some(b) = inner.bindings.get(bid) else {
            return;
        };
        if inner
            .peers
            .get(&b.connector)
            .is_none_or(|p| p.generation != generation)
        {
            return;
        }
        inner.inflight.remove(bid);
        let Some(row) = inner.rows.iter_mut().find(|r| r.id == rid) else {
            return;
        };
        if row.status == "read" {
            return;
        }
        let status = answer.as_ref().ok().and_then(valid_delivery_ack);
        let new_status = match status {
            Some("accepted") => "delivered",
            Some("unknown") => "unconfirmed",
            Some("unsupported") => "not_sent",
            _ => "pending",
        };
        row.status = new_status.into();
        row.reason = match status {
            Some("unknown") => answer
                .as_ref()
                .ok()
                .and_then(|v| v.get("detail"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            Some("unsupported") => Some("unsupported".into()),
            _ => None,
        };
        if new_status == "pending" {
            row.attempts += 1;
            row.next_attempt = seconds() + [2, 5, 15, 60][row.attempts.min(4) - 1];
        }
        let sid = row.session.clone();
        let payload = row.payload.clone().unwrap_or_default();
        if let Some(c) = inner.browsers.get_mut(&sid) {
            if new_status == "delivered" {
                c.sent += 1;
            }
            let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":rid,"thread_id":payload["thread_id"],"session_id":sid,"status":new_status}}));
        }
    }
    pub async fn pump(self: std::sync::Arc<Self>) {
        loop {
            for (bid, rid, peer, data) in self.pending_delivery() {
                let room = self.clone();
                tokio::spawn(async move {
                    let answer = peer.request("input.deliver", data, ACK_TIMEOUT).await;
                    room.settle_delivery(&bid, &rid, &peer.generation, answer);
                });
            }
            {
                let mut inner = self.inner.lock().expect("room lock");
                let clients: Vec<String> = inner.browsers.keys().cloned().collect();
                for sid in clients {
                    dispatch_client(&mut inner, &sid);
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}
fn queue_input_locked(inner: &mut Inner, draft: InputDraft<'_>) -> Value {
    let InputDraft {
        row_id,
        text,
        session_id,
        revision,
        thread_id,
        binding_id,
        title,
        language,
        message_id,
    } = draft;
    let payload = json!({"thread_id":thread_id,"text":text,"message_id":message_id,"session_id":session_id,
        "history_id":row_id,"revision":revision,"binding_id":binding_id,"title":title});
    inner.seq += 1;
    inner.rows.push_back(Row {
        seq: inner.seq,
        id: row_id.clone(),
        thread: thread_id.into(),
        role: "user",
        text: text.into(),
        name: Some(crate::messages::render(
            &crate::messages::LocalizedMessage::new("room.you"),
            language,
        )),
        session: session_id.into(),
        revision,
        time: millis(),
        status: "pending".into(),
        reason: None,
        language: None,
        offline: None,
        payload: Some(payload),
        queued_at: seconds(),
        attempts: 0,
        next_attempt: 0,
    });
    trim_rows(inner);
    if let Some(browser) = inner.browsers.get(session_id) {
        let _=browser.sender.try_send(json!({"type":"voice-input-receipt","data":{
            "revision":revision,"history_id":row_id,"thread_id":thread_id,"session_id":session_id,"status":"pending"}}));
    }
    json!({"accepted":true,"id":row_id,"revision":revision})
}
fn trim_rows(inner: &mut Inner) {
    while inner.rows.len() > MAX_HISTORY
        && inner
            .rows
            .front()
            .is_some_and(|r| !matches!(r.status.as_str(), "pending" | "sending"))
    {
        if let Some(row) = inner.rows.pop_front() {
            inner.utterances.retain(|_, u| u.row_id != row.id);
        }
    }
}
fn reachability(binding: &Binding, language: &str) -> Value {
    let render = |key: &'static str| {
        crate::messages::render(&crate::messages::LocalizedMessage::new(key), language)
    };
    if !binding.live {
        return json!({"state":"offline","detail":render("room.reach_offline")});
    }
    if binding
        .inbound
        .as_ref()
        .is_some_and(|inbound| inbound.get("ok") == Some(&Value::Bool(false)))
    {
        return json!({"state":"holding","detail":render("room.reach_holding"),"remedy":Value::Null});
    }
    if binding.capabilities.get("deliver").and_then(Value::as_str) == Some("unsupported") {
        return json!({"state":"holding","detail":render("room.reach_unsupported")});
    }
    json!({"state":"listening","detail":Value::Null})
}
fn status_rank(status: &str) -> u8 {
    match status {
        "failed" => 2,
        "interrupted" => 3,
        "queued" => 4,
        "waiting_for_turn" => 5,
        "playing" => 7,
        "playback_finished" => 8,
        _ => 0,
    }
}
fn sync_row(inner: &mut Inner, row_id: &str, changed: &str, reason: Option<&str>) {
    let best = inner
        .utterances
        .values()
        .find(|record| record.row_id == row_id)
        .and_then(|record| {
            record
                .clients
                .values()
                .map(|(_, status)| status.as_str())
                .max_by_key(|status| status_rank(status))
        })
        .unwrap_or(changed)
        .to_owned();
    if let Some(row) = inner.rows.iter_mut().find(|row| row.id == row_id) {
        row.status = best.clone();
        row.reason = if best == changed {
            reason.map(str::to_owned)
        } else {
            None
        };
    }
}
fn dispatch_client(inner: &mut Inner, sid: &str) {
    loop {
        let Some(browser) = inner.browsers.get(sid) else {
            return;
        };
        if browser.active.is_some() || browser.speaking {
            return;
        }
        let Some(uid) = browser.pending.front().cloned() else {
            return;
        };
        let entry = inner.utterances.get(&uid).and_then(|record| {
            record
                .clients
                .get(sid)
                .map(|(revision, status)| (record.row_id.clone(), *revision, status.clone()))
        });
        let Some((row_id, revision, status)) = entry else {
            inner
                .browsers
                .get_mut(sid)
                .expect("browser present")
                .pending
                .pop_front();
            continue;
        };
        if status == "waiting_for_turn" {
            return;
        }
        if status != "queued" {
            inner
                .browsers
                .get_mut(sid)
                .expect("browser present")
                .pending
                .pop_front();
            continue;
        }
        let row = inner.rows.iter().find(|r| r.id == row_id).map(|r| {
            (
                r.thread.clone(),
                r.text.clone(),
                r.language.clone(),
                r.revision,
            )
        });
        let Some((thread, text, language, reply_revision)) = row else {
            inner
                .browsers
                .get_mut(sid)
                .expect("browser present")
                .pending
                .pop_front();
            continue;
        };
        let browser = inner.browsers.get_mut(sid).expect("browser present");
        if browser.revision != revision
            || browser.target.as_ref().is_none_or(|t| t.thread != thread)
        {
            browser.pending.pop_front();
            if let Some(record) = inner.utterances.get_mut(&uid) {
                if let Some(entry) = record.clients.get_mut(sid) {
                    entry.1 = "interrupted".into();
                }
            }
            sync_row(inner, &row_id, "interrupted", Some("focus_changed"));
            continue;
        }
        let event = json!({"type":"voice-speech","data":{"session_id":sid,"utterance_id":uid,"revision":revision,"reply_revision":reply_revision,"thread_id":thread,"text":text,"language":language,"history_id":row_id}});
        if browser.sender.try_send(event).is_err() {
            return;
        }
        browser.pending.pop_front();
        browser.active = Some(uid);
        return;
    }
}
fn interrupt_client(inner: &mut Inner, sid: &str, reason: &str) {
    let mut rows = Vec::new();
    for record in inner.utterances.values_mut() {
        if let Some(entry) = record.clients.get_mut(sid) {
            if matches!(entry.1.as_str(), "queued" | "waiting_for_turn" | "playing") {
                entry.1 = "interrupted".into();
                if record.clients.values().all(|(_, status)| {
                    !matches!(status.as_str(), "queued" | "waiting_for_turn" | "playing")
                }) {
                    rows.push(record.row_id.clone());
                }
            }
        }
    }
    for row_id in rows {
        if let Some(row) = inner.rows.iter_mut().find(|r| r.id == row_id) {
            if row.status != "playback_finished" {
                row.status = "interrupted".into();
                row.reason = Some(reason.into());
            }
        }
    }
    if let Some(browser) = inner.browsers.get_mut(sid) {
        browser.pending.clear();
        browser.active = None;
    }
}
fn hold_client(inner: &mut Inner, sid: &str, revision: u64) {
    let active = inner.browsers.get_mut(sid).and_then(|b| b.active.take());
    let mut waiting = Vec::new();
    let mut interrupted = Vec::new();
    for (uid, record) in &mut inner.utterances {
        if let Some(entry) = record.clients.get_mut(sid) {
            match entry.1.as_str() {
                "queued" | "waiting_for_turn" => {
                    entry.0 = revision;
                    entry.1 = "waiting_for_turn".into();
                    waiting.push((uid.clone(), record.row_id.clone()));
                }
                "playing" => {
                    entry.1 = "interrupted".into();
                    interrupted.push(record.row_id.clone());
                }
                _ => {}
            }
        }
    }
    if let Some(uid) = active {
        if waiting.iter().any(|(id, _)| id == &uid) {
            if let Some(browser) = inner.browsers.get_mut(sid) {
                browser.pending.push_front(uid);
            }
        }
    }
    for (_, row_id) in waiting {
        sync_row(inner, &row_id, "waiting_for_turn", Some("user_speaking"));
    }
    for row_id in interrupted {
        sync_row(inner, &row_id, "interrupted", Some("newer_turn"));
    }
}
fn engine(value: Option<&Value>) -> Option<Value> {
    let obj = value?.as_object()?;
    let mut out = Map::new();
    for key in ["model", "effort", "thinking"] {
        if let Some(v) = obj
            .get(key)
            .filter(|v| !v.is_null() && v.as_str() != Some(""))
        {
            out.insert(
                key.into(),
                json!(v
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
                    .chars()
                    .take(60)
                    .collect::<String>()),
            );
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}
fn capabilities(value: Option<&Value>, experimental: Option<&Value>) -> Value {
    let mut result = Map::new();
    for key in [
        "deliver",
        "inspectInbound",
        "working",
        "endOfTurn",
        "sessionIdentity",
    ] {
        let status = value
            .and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .filter(|s| ["supported", "unsupported"].contains(s))
            .unwrap_or("unknown");
        result.insert(key.into(), json!(status));
    }
    let marked = experimental
        .or_else(|| value.and_then(|v| v.get("experimental")))
        .and_then(Value::as_array);
    if let Some(marked) = marked {
        let names: Vec<&str> = [
            "deliver",
            "inspectInbound",
            "working",
            "endOfTurn",
            "sessionIdentity",
        ]
        .into_iter()
        .filter(|key| result[*key] == "supported" && marked.iter().any(|v| v.as_str() == Some(key)))
        .collect();
        if !names.is_empty() {
            result.insert("experimental".into(), json!(names));
        }
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_voice_turn_uses_captured_focus_and_shared_outbox() {
        let directory = tempfile::tempdir().unwrap();
        let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
        let (requests, _receiver) = mpsc::channel(4);
        let (stop, _stopped) = watch::channel(false);
        room.attach(
            "connector",
            ConnectorPeer {
                generation: id(),
                sender: requests,
                stop,
            },
        );
        room.register(
            "connector",
            &json!({"thread":"old-thread","harness":"codex"}),
        )
        .unwrap();
        room.register(
            "connector",
            &json!({"thread":"new-thread","harness":"codex"}),
        )
        .unwrap();
        let (events, _received) = mpsc::channel(8);
        let sid = room.join("device".into(), "en".into(), events).unwrap();
        room.select(&sid, "old-thread").unwrap();
        let turn = room.begin_turn(&sid).unwrap();
        assert_eq!(turn.thread_id.as_deref(), Some("old-thread"));
        room.select(&sid, "new-thread").unwrap();
        let accepted = room
            .queue_voice_input(&turn, "Words for the old thread")
            .unwrap();
        assert_eq!(accepted["accepted"], true);
        assert_eq!(accepted["id"], format!("{sid}:user-turn:{}", turn.revision));
        let rows = room.history(Some("old-thread"));
        assert_eq!(rows["messages"][0]["text"], "Words for the old thread");
        assert_eq!(rows["messages"][0]["status"], "pending");
        assert_eq!(
            room.history(Some("new-thread"))["messages"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        let delivery = room.pending_delivery();
        assert_eq!(delivery.len(), 1);
        assert_eq!(delivery[0].3["thread"], "old-thread");
        assert_eq!(
            room.queue_voice_input(&turn, "Words for the old thread")
                .unwrap(),
            accepted
        );
    }

    #[test]
    fn host_agent_peer_is_latest_live_connection() {
        let directory = tempfile::tempdir().unwrap();
        let room = Room::load(PrivateDir::open(directory.path().join("private")).unwrap()).unwrap();
        for (cid, generation) in [
            ("first", "first-1"),
            ("second", "second-1"),
            ("first", "first-2"),
        ] {
            let (requests, _receiver) = mpsc::channel(1);
            let (stop, _stopped) = watch::channel(false);
            room.attach(
                cid,
                ConnectorPeer {
                    generation: generation.into(),
                    sender: requests,
                    stop,
                },
            );
            assert_eq!(room.connector_peer().unwrap().generation, generation);
        }
        room.detach("first", "first-1");
        assert_eq!(room.connector_peer().unwrap().generation, "first-2");
        room.detach("first", "first-2");
        assert_eq!(room.connector_peer().unwrap().generation, "second-1");
    }
}
