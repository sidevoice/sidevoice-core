//! One owner for connector credentials, bindings and process-local conversation state.
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use rand::RngCore;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::storage::PrivateDir;

const MAX_HISTORY: usize = 2000;
const MAX_BROWSERS: usize = 8;
const INPUT_TTL: u64 = 600;
const ACK_TIMEOUT: Duration = Duration::from_secs(60);

fn seconds() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }
fn millis() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }
fn hash(token: &str) -> String { format!("{:x}", Sha256::digest(token.as_bytes())) }
fn id() -> String { Uuid::new_v4().to_string() }
fn field<'a>(value: &'a Value, name: &str) -> &'a str { value.get(name).and_then(Value::as_str).unwrap_or("") }
fn valid_thread(thread: &str) -> bool { !thread.is_empty() && thread.len() <= 200 && thread.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)) }
fn valid_delivery_ack(value:&Value)->Option<&str>{
    let fields=value.as_object()?;
    if serde_json::to_vec(value).ok()?.len()>4096||fields.keys().any(|k|!["status","detail","error"].contains(&k.as_str())){return None;}
    for key in ["detail","error"]{if fields.get(key).is_some_and(|v|v.as_str().is_none_or(|s|s.len()>1000)){return None;}}
    let status=field(value,"status");["accepted","unknown","unsupported","failed","unknown_binding"].contains(&status).then_some(status)
}

#[derive(Clone, Debug)]
pub struct RoomError { pub status: u16, pub key: &'static str }
impl RoomError { fn new(status: u16, key: &'static str) -> Self { Self { status, key } } }

#[derive(Debug)]
pub struct PeerError;
pub struct PeerRequest { pub method: String, pub params: Value, pub answer: Option<oneshot::Sender<Result<Value, PeerError>>> }
#[derive(Clone)]
pub struct ConnectorPeer { pub generation: String, pub sender: mpsc::Sender<PeerRequest> }
impl ConnectorPeer {
    pub async fn send(&self, method: &str, params: Value) -> Result<(), PeerError> {
        self.sender.send(PeerRequest { method: method.into(), params, answer: None }).await.map_err(|_| PeerError)
    }
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, PeerError> {
        let (tx, rx) = oneshot::channel();
        self.sender.send(PeerRequest { method: method.into(), params, answer: Some(tx) }).await.map_err(|_| PeerError)?;
        tokio::time::timeout(timeout, rx).await.map_err(|_| PeerError)?.map_err(|_| PeerError)?
    }
}

struct Browser { device: String, sender: mpsc::Sender<Value>, target: Option<Target>, revision: u64,
    turn_revision: u64, speaking: bool, sent: u64, active: Option<String> }
#[derive(Clone)] struct Target { thread: String, title: Option<String>, binding_id: String }
impl Target { fn view(&self) -> Value { json!({"thread_id": self.thread, "title": self.title, "binding_id": self.binding_id}) } }
struct Binding { id: String, connector: String, thread: String, harness: String, title: Option<String>,
    created: u64, active: bool, live: bool, inbound: Option<Value>, capabilities: Value,
    engine: Option<Value>, route: Option<String> }
impl Binding {
    fn view(&self) -> Value { json!({"id": self.id, "connector": self.connector, "thread": self.thread,
        "harness": self.harness, "title": self.title, "created": self.created, "active": self.active as u8,
        "inbound": self.inbound, "capabilities": self.capabilities, "engine": self.engine, "route": self.route,
        "connected": self.live}) }
}
struct Row { seq: u64, id: String, thread: String, role: &'static str, text: String, name: Option<String>,
    session: String, revision: u64, time: u64, status: String, reason: Option<String>,
    language: Option<String>, offline: Option<Value>, payload: Option<Value>, queued_at: u64,
    attempts: usize, next_attempt: u64 }
impl Row {
    fn view(&self) -> Value { json!({"seq": self.seq, "id": self.id, "thread": self.thread, "role": self.role,
        "text": self.text, "name": self.name, "session": self.session, "revision": self.revision,
        "time": self.time, "status": self.status, "audio_reason": self.reason, "offline": self.offline}) }
}
struct UtteranceRecord { row_id: String, clients: HashMap<String, (u64, String)> }
struct Inner { connectors: Map<String, Value>, pairing: HashMap<String, u64>, peers: HashMap<String, ConnectorPeer>,
    bindings: HashMap<String, Binding>, browsers: HashMap<String, Browser>, sessions: VecDeque<String>,
    rows: VecDeque<Row>, utterances: HashMap<String, UtteranceRecord>, seq: u64,
    working: HashMap<String, bool>, inflight: HashMap<String, String> }

pub struct Room { dir: PrivateDir, inner: Mutex<Inner> }
impl Room {
    pub fn load(dir: PrivateDir) -> io::Result<Self> {
        let state = match dir.read_json("room-state.json")? { Some(v) => v, None => dir.import_legacy_connectors()? };
        let connectors = state.get("connectors").and_then(Value::as_object).cloned().unwrap_or_default();
        Ok(Self { dir, inner: Mutex::new(Inner { connectors, pairing: HashMap::new(), peers: HashMap::new(),
            bindings: HashMap::new(), browsers: HashMap::new(), sessions: VecDeque::new(), rows: VecDeque::new(), utterances: HashMap::new(),
            seq: 0, working: HashMap::new(), inflight: HashMap::new() }) })
    }
    fn save(&self, inner: &Inner) -> io::Result<()> {
        self.dir.write_json("room-state.json", &json!({"connectors": inner.connectors}))
    }
    pub fn local_credential(&self) -> io::Result<(String, String)> {
        let mut inner = self.inner.lock().expect("room lock");
        if let Some(saved) = self.dir.read_json("connector-credential.json")? {
            let cid = field(&saved, "connector_id"); let token = field(&saved, "token");
            if self.credential_locked(&inner, cid, token) == "paired" { return Ok((cid.into(), token.into())); }
        }
        let cid = id(); let mut bytes = [0u8; 32]; rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let at = seconds();
        inner.connectors.insert(cid.clone(), json!({"token_hash": hash(&token), "created": at, "last_seen": at, "revoked": 0}));
        self.save(&inner)?;
        self.dir.write_json("connector-credential.json", &json!({"connector_id": cid, "token": token}))?;
        Ok((cid, token))
    }
    fn credential_locked(&self, inner: &Inner, cid: &str, token: &str) -> &'static str {
        if cid.is_empty() || token.is_empty() { return "unknown"; }
        let Some(entry) = inner.connectors.get(cid) else { return "unknown"; };
        let expected = field(entry, "token_hash");
        if expected.is_empty() || expected != hash(token) { return "unknown"; }
        if entry.get("revoked").is_some_and(|v| v == true || v.as_i64().unwrap_or_default() != 0) { "revoked" } else { "paired" }
    }
    pub fn authenticate_connector(&self, cid: &str, token: &str, identity: &Value) -> bool {
        let mut inner = self.inner.lock().expect("room lock");
        if self.credential_locked(&inner, cid, token) != "paired" { return false; }
        if let Some(entry) = inner.connectors.get_mut(cid).and_then(Value::as_object_mut) {
            entry.insert("last_seen".into(), json!(seconds()));
            for (key, limit) in [("host", 200), ("platform", 60), ("version", 40)] {
                if let Some(s) = identity.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                    entry.insert(key.into(), json!(s.trim().chars().take(limit).collect::<String>()));
                }
            }
            if let Some(names) = identity.get("harnesses").and_then(Value::as_array) {
                entry.insert("harnesses".into(), json!(names.iter().take(8).filter_map(Value::as_str).map(|s| s.trim().chars().take(40).collect::<String>()).collect::<Vec<_>>()));
            }
        }
        self.save(&inner).is_ok()
    }
    pub fn pairing_code(&self) -> String {
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut bytes = [0u8; 12]; rand::rngs::OsRng.fill_bytes(&mut bytes);
        let code: String = bytes.iter().map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect();
        let mut inner = self.inner.lock().expect("room lock");
        inner.pairing.retain(|_, expiry| *expiry >= seconds());
        inner.pairing.insert(code.clone(), seconds()+180);
        format!("{}-{}-{}", &code[..4], &code[4..8], &code[8..])
    }
    pub fn redeem_pairing(&self, code: &str, identity: &Value) -> io::Result<Option<(String, String)>> {
        let normalized: String = code.chars().filter(|c| !" -_.".contains(*c)).map(|c| match c.to_ascii_uppercase() {'O'=>'0','I'|'L'=>'1',other=>other}).collect();
        let mut inner = self.inner.lock().expect("room lock");
        if inner.pairing.remove(&normalized).is_none_or(|expiry| expiry < seconds()) { return Ok(None); }
        let cid = id(); let mut bytes = [0u8; 32]; rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut record = json!({"token_hash": hash(&token), "created": seconds(), "last_seen": seconds(), "revoked": 0});
        if let Some(obj) = record.as_object_mut() { for key in ["host", "platform", "version", "harnesses"] {
            if let Some(value) = identity.get(key) { obj.insert(key.into(), value.clone()); }
        }}
        inner.connectors.insert(cid.clone(), record); self.save(&inner)?; Ok(Some((cid, token)))
    }
    pub fn binding_views(&self) -> Value {
        let inner=self.inner.lock().expect("room lock");
        json!(inner.bindings.values().filter(|b|b.active).map(Binding::view).collect::<Vec<_>>())
    }
    pub fn paired_connectors(&self) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let mut entries: Vec<Value> = inner.connectors.iter().map(|(id, row)| {
            let mut result = row.as_object().cloned().unwrap_or_default(); result.remove("token_hash");
            result.insert("id".into(), json!(id)); result.insert("connected".into(), json!(inner.peers.contains_key(id))); Value::Object(result)
        }).collect();
        entries.sort_by_key(|e| e.get("created").and_then(Value::as_u64).unwrap_or_default()); json!(entries)
    }
    pub fn attach(&self, cid: &str, peer: ConnectorPeer) -> Option<ConnectorPeer> {
        let mut inner = self.inner.lock().expect("room lock");
        let old = inner.peers.insert(cid.into(), peer);
        for binding in inner.bindings.values_mut().filter(|b| b.connector == cid && b.active) { binding.live = false; }
        old
    }
    pub fn detach(&self, cid: &str, generation: &str) {
        let mut inner = self.inner.lock().expect("room lock");
        if inner.peers.get(cid).is_none_or(|p| p.generation != generation) { return; }
        inner.peers.remove(cid);
        let ids: Vec<String> = inner.bindings.values_mut().filter(|b| b.connector == cid).map(|b| { b.live=false; b.id.clone() }).collect();
        for bid in ids { if let Some(row_id) = inner.inflight.remove(&bid) { if let Some(row) = inner.rows.iter_mut().find(|r| r.id == row_id) { row.status="pending".into(); row.next_attempt=0; } } }
    }
    pub fn connector_peer(&self) -> Option<ConnectorPeer> {
        let inner = self.inner.lock().expect("room lock");
        inner.peers.values().next().cloned()
    }
    pub fn register(&self, cid: &str, data: &Value) -> Result<Value, RoomError> {
        let thread = field(data, "thread");
        if !valid_thread(thread) { return Err(RoomError::new(400,"room.thread_invalid")); }
        let mut inner = self.inner.lock().expect("room lock");
        if !inner.peers.contains_key(cid) { return Err(RoomError::new(409,"room.connector_disconnected")); }
        let requested = field(data,"binding_id");
        if let Some(existing) = inner.bindings.get(requested) { if existing.connector != cid { return Err(RoomError::new(409,"room.binding_foreign")); } }
        let existing = if !requested.is_empty() && inner.bindings.contains_key(requested) { Some(requested.to_owned()) }
            else { inner.bindings.values().filter(|b| b.connector == cid && b.thread == thread && b.active).max_by_key(|b| b.created).map(|b| b.id.clone()) };
        let bid = existing.unwrap_or_else(id);
        let capabilities = capabilities(data.get("capabilities"), data.get("experimental"));
        let engine = engine(data.get("engine"));
        let route = match field(data,"route") { "cursor-editor-bridge"|"cursor-editor-view"|"cursor-cli-persist"|"cursor-cli" => Some(field(data,"route").into()), _=>None };
        let title = data.get("title").and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| s.chars().take(200).collect());
        let harness = data.get("harness").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("unknown").chars().take(40).collect();
        let inbound = data.get("inbound").filter(|v| v.is_object()).cloned();
        let binding = inner.bindings.entry(bid.clone()).or_insert_with(|| Binding { id: bid.clone(), connector: cid.into(), thread: thread.into(),
            harness: String::new(), title: None, created: seconds(), active: true, live: true,
            inbound: None, capabilities: Value::Null, engine: None, route: None });
        binding.harness=harness; binding.active=true; binding.live=true; if title.is_some() { binding.title=title; }
        if inbound.is_some() { binding.inbound=inbound; } binding.capabilities=capabilities;
        if engine.is_some() { binding.engine=engine; } binding.route=route;
        inner.working.remove(thread);
        Ok(json!({"client_ref": data.get("client_ref"), "binding_id": bid, "thread": thread}))
    }
    pub fn unregister(&self, cid: &str, bid: &str) {
        let mut inner=self.inner.lock().expect("room lock");
        if let Some(b)=inner.bindings.get_mut(bid).filter(|b| b.connector==cid) { b.active=false; b.live=false; let thread=b.thread.clone(); inner.working.remove(&thread); }
    }
    pub fn close_channel(&self,thread:&str)->Result<(Value,Option<(ConnectorPeer,Value)>),RoomError>{
        if !valid_thread(thread){return Err(RoomError::new(400,"room.thread_invalid"));}
        let mut inner=self.inner.lock().expect("room lock");
        let binding_id=inner.bindings.values().filter(|b|b.thread==thread&&b.active).max_by_key(|b|b.created).map(|b|b.id.clone());
        if binding_id.is_none()&&!inner.rows.iter().any(|r|r.thread==thread){return Err(RoomError::new(409,"room.conversation_missing"));}
        let notify=binding_id.as_ref().and_then(|bid|inner.bindings.get(bid)).and_then(|b|inner.peers.get(&b.connector).cloned().map(|p|(p,json!({"binding_id":b.id,"thread":thread,"reason":"closed_from_room"}))));
        if let Some(bid)=binding_id.as_ref(){if let Some(b)=inner.bindings.get_mut(bid){b.active=false;b.live=false;}inner.inflight.remove(bid);}
        inner.working.remove(thread);
        for row in inner.rows.iter_mut().filter(|r|r.role=="user"&&r.thread==thread&&matches!(r.status.as_str(),"pending"|"sending")){
            row.status="not_sent".into();row.reason=Some("channel_closed".into());
        }
        for browser in inner.browsers.values_mut().filter(|b|b.target.as_ref().is_some_and(|t|t.thread==thread)){
            browser.revision+=1;browser.target=Some(Target{thread:String::new(),title:None,binding_id:id()});browser.active=None;browser.speaking=false;
            let _=browser.sender.try_send(json!({"type":"voice-cancel","data":{"revision":browser.revision}}));
        }
        Ok((json!({"status":"closed","binding_id":binding_id}),notify))
    }
    pub fn participants(&self, session: Option<&str>) -> Value {
        let inner=self.inner.lock().expect("room lock");
        let selected= session.and_then(|sid| inner.browsers.get(sid)).and_then(|b| b.target.as_ref()).map(|t| t.thread.as_str());
        json!(inner.bindings.values().filter(|b| b.active).map(|b| {
            let host=inner.connectors.get(&b.connector).and_then(|c| c.get("host"));
            json!({"thread_id":b.thread,"title":b.title.clone().unwrap_or_else(||crate::messages::render(&crate::messages::LocalizedMessage::new("room.conversation_title").with_param("id",b.thread.chars().take(8).collect::<String>()),"en")),
                "harness":b.harness,"available":b.live,"machine":{"id":b.connector,"host":host},
                "capabilities":b.capabilities,"engine":b.engine,"route":b.route,
                "reach":{"state":if b.live {"listening"} else {"offline"},"detail":Value::Null},"selected":selected==Some(b.thread.as_str())})
        }).collect::<Vec<_>>())
    }
    pub fn join(&self, device: String, sender: mpsc::Sender<Value>) -> Result<String, RoomError> {
        let mut inner=self.inner.lock().expect("room lock");
        let max=std::env::var("VOICE_MAX_BROWSERS").ok().and_then(|v|v.parse::<usize>().ok()).unwrap_or(MAX_BROWSERS).max(1);
        if inner.browsers.len()>=max { return Err(RoomError::new(429,"room.full")); }
        let sid=id(); inner.sessions.push_back(sid.clone()); if inner.sessions.len()>64 {inner.sessions.pop_front();}
        inner.browsers.insert(sid.clone(),Browser { device,sender,target:None,revision:0,turn_revision:0,speaking:false,sent:0,active:None }); Ok(sid)
    }
    pub fn leave(&self, sid: &str) { self.inner.lock().expect("room lock").browsers.remove(sid); }
    pub fn admission(&self) -> Value { let inner=self.inner.lock().expect("room lock"); let max=std::env::var("VOICE_MAX_BROWSERS").ok().and_then(|v|v.parse::<usize>().ok()).unwrap_or(MAX_BROWSERS).max(1);
        json!({"admitted":inner.browsers.len()<max,"reason":if inner.browsers.len()>=max {Some("room_is_full")} else {None},
        "message":if inner.browsers.len()>=max {Some(crate::messages::render(&crate::messages::LocalizedMessage::new("room.full"),"en"))} else {None},"clients":inner.browsers.len(),"max":max}) }
    pub fn select(&self, sid:&str, thread:&str) -> Result<Value,RoomError> {
        if !valid_thread(thread) {return Err(RoomError::new(400,"room.thread_invalid"));}
        let mut inner=self.inner.lock().expect("room lock");
        let Some(binding)=inner.bindings.values().filter(|b|b.active&&b.thread==thread).max_by_key(|b|b.created) else {return Err(RoomError::new(409,"room.conversation_disconnected"));};
        let title=binding.title.clone(); let Some(client)=inner.browsers.get_mut(sid) else {return Err(RoomError::new(409,"room.browser_absent"));};
        if client.target.as_ref().is_some_and(|t|t.thread==thread) {return Ok(json!({"status":"already_active","binding":client.target.as_ref().map(Target::view)}));}
        client.revision+=1; client.active=None; client.speaking=false; client.sent=0;
        let target=Target{thread:thread.into(),title,binding_id:id()}; let view=target.view(); client.target=Some(target);
        Ok(json!({"status":"activated","binding":view}))
    }
    pub fn deselect(&self,sid:&str,bid:&str)->Result<Value,RoomError>{let mut inner=self.inner.lock().expect("room lock"); let Some(c)=inner.browsers.get_mut(sid) else{return Err(RoomError::new(409,"room.browser_absent"));};
        if c.target.as_ref().is_none_or(|t|t.binding_id!=bid){return Err(RoomError::new(409,"room.focus_changed"));}
        c.revision+=1;c.active=None;c.speaking=false;let target=Target{thread:String::new(),title:None,binding_id:id()};let view=target.view();c.target=Some(target);Ok(json!({"status":"activated","binding":view}))}
    pub fn begin_turn(&self,sid:&str)->Result<(u64,Option<String>),RoomError>{let mut inner=self.inner.lock().expect("room lock");let Some(c)=inner.browsers.get_mut(sid) else{return Err(RoomError::new(409,"room.browser_absent"));};c.revision+=1;c.turn_revision=c.revision;c.speaking=true;c.active=None;Ok((c.revision,c.target.as_ref().map(|t|t.thread.clone())))}
    pub fn finish_turn(&self,sid:&str,revision:u64){if let Some(c)=self.inner.lock().expect("room lock").browsers.get_mut(sid){if c.turn_revision==revision{c.speaking=false;}}}
    pub fn send_text(&self,text:&str,sid:&str,thread:&str,bid:&str,message_id:&str)->Result<Value,RoomError>{
        let row_id=format!("{sid}:user-text:{message_id}");let mut inner=self.inner.lock().expect("room lock");
        if let Some(row)=inner.rows.iter().find(|r|r.id==row_id){return if row.text==text&&row.thread==thread {Ok(json!({"accepted":true,"id":row_id,"revision":row.revision}))} else {Err(RoomError::new(409,"room.message_conflict"))};}
        let Some(c)=inner.browsers.get(sid) else{return Err(RoomError::new(409,"room.focus_changed"));};
        if c.target.as_ref().is_none_or(|t|t.thread!=thread||t.binding_id!=bid){return Err(RoomError::new(409,"room.focus_changed"));}
        if text.trim().is_empty(){return Err(RoomError::new(422,"room.text_empty"));}
        let revision=c.revision;let sender=c.sender.clone();let payload=json!({"thread_id":thread,"text":text,"message_id":message_id,"session_id":sid,"history_id":row_id,"revision":revision,"binding_id":bid,"title":c.target.as_ref().and_then(|t|t.title.clone())});
        inner.seq+=1;let seq=inner.seq;inner.rows.push_back(Row{seq,id:row_id.clone(),thread:thread.into(),role:"user",text:text.into(),name:Some(crate::messages::render(&crate::messages::LocalizedMessage::new("room.you"),"en")),session:sid.into(),revision,time:millis(),status:"pending".into(),reason:None,language:None,offline:None,payload:Some(payload.clone()),queued_at:seconds(),attempts:0,next_attempt:0});trim_rows(&mut inner);
        let _=sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":revision,"history_id":row_id,"thread_id":thread,"session_id":sid,"status":"pending"}}));
        Ok(json!({"accepted":true,"id":row_id,"revision":revision}))
    }
    pub fn history(&self,thread:Option<&str>)->Value{let inner=self.inner.lock().expect("room lock");json!({"messages":inner.rows.iter().filter(|r|thread.is_none_or(|t|r.thread==t)).rev().take(1000).collect::<Vec<_>>().into_iter().rev().map(Row::view).collect::<Vec<_>>()})}
    pub fn reply_language(&self,row_id:&str)->Option<String>{self.inner.lock().expect("room lock").rows.iter().find(|r|r.id==row_id).and_then(|r|r.language.clone())}
    pub fn snapshot(&self,sid:Option<&str>)->Value{let inner=self.inner.lock().expect("room lock");let c=sid.and_then(|s|inner.browsers.get(s));json!({"binding":c.and_then(|b|b.target.as_ref()).filter(|t|!t.thread.is_empty()).map(Target::view),
        "room":{"revision":c.map_or(0,|b|b.revision),"speaking":c.is_some_and(|b|b.speaking),"switching":false,"clients":inner.browsers.len(),"utterances":[],"audio_reports":[],"client_errors":[]},
        "clients":inner.browsers.iter().map(|(id,b)|json!({"id":id,"device_id":b.device,"connected":true,"user_speaking":b.speaking,"turn_revision":b.turn_revision,"transport":"pcm","transcription":Value::Null})).collect::<Vec<_>>(),
        "call":c.map(|b|json!({"id":sid,"target":b.target.as_ref().map(Target::view).unwrap_or(json!({})),"connected":true,"user_speaking":b.speaking,"error":Value::Null,"sent":b.sent,"last_delivery":Value::Null,"revision":b.revision,"utterances":[],"mic":Value::Null,"mic_settings":Value::Null,"transcription":Value::Null,"audio_health":Value::Null,"speech_filter":{}}))})}
    pub fn publish(&self,p:&Value,v3:bool)->Value{
        let sid=field(p,"session_id");let thread=field(p,"thread_id");let uid=field(p,"utterance_id");let text=field(p,"text");let revision=p.get("revision").and_then(Value::as_u64).unwrap_or(0);
        let mut inner=self.inner.lock().expect("room lock");let row_id=format!("{sid}:voice:{uid}");
        if let Some(row)=inner.rows.iter().find(|r|r.id==row_id){return json!({"status":row.status,"text_saved":true,"utterance_id":uid});}
        if text.is_empty()||text.len()>6000||thread.is_empty(){return json!({"status":"rejected","error":"room.speech_invalid","terminal":v3,"reason_code":"application_refusal"});}
        let audience:Vec<String>=inner.browsers.iter().filter(|(_,b)|b.target.as_ref().is_some_and(|t|t.thread==thread)).map(|(id,_)|id.clone()).collect();
        let asker=inner.browsers.get(sid);let known=inner.sessions.iter().any(|s|s==sid);
        let reason=if !known {Some("session_changed")} else if asker.is_none(){Some("call_ended")} else if asker.is_some_and(|b|b.target.as_ref().is_none_or(|t|t.thread!=thread)){Some("focus_changed")} else if asker.is_some_and(|b|b.revision!=revision){Some("newer_turn")} else if asker.is_some_and(|b|b.speaking){Some("user_speaking")} else {None};
        let status=if reason.is_some_and(|r|!["newer_turn","user_speaking"].contains(&r))||audience.is_empty(){"text_only"}else{"queued"};
        inner.seq+=1;let seq=inner.seq;inner.rows.push_back(Row{seq,id:row_id.clone(),thread:thread.into(),role:"assistant",text:text.into(),name:None,session:sid.into(),revision,time:millis(),status:status.into(),reason:reason.map(str::to_owned),language:p.get("language").and_then(Value::as_str).map(str::to_owned),offline:None,payload:None,queued_at:seconds(),attempts:0,next_attempt:0});trim_rows(&mut inner);
        if status=="queued" {let mut clients=HashMap::new();for listener in audience { if let Some(c)=inner.browsers.get(&listener){let _=c.sender.try_send(json!({"type":"voice-speech","data":{"session_id":listener,"utterance_id":uid,"revision":c.revision,"reply_revision":revision,"thread_id":thread,"text":text,"language":p.get("language"),"history_id":row_id}}));clients.insert(listener,(c.revision,"queued".into()));}}inner.utterances.insert(uid.into(),UtteranceRecord{row_id,clients});}
        if status=="text_only" {json!({"status":"text_only","text_saved":true,"reason":reason.unwrap_or("call_ended")})}else{json!({"status":"queued","utterance_id":uid,"session_id":sid,"revision":revision,"text_saved":true})}
    }
    pub fn connector_speech(&self,cid:&str,p:&Value,v3:bool)->Value{
        let bid=field(p,"binding_id");
        let thread={let inner=self.inner.lock().expect("room lock");inner.bindings.get(bid).filter(|b|b.connector==cid&&b.live).map(|b|b.thread.clone())};
        let Some(thread)=thread else{return if v3{json!({"status":"unknown_binding","event_id":p.get("event_id"),"utterance_id":p.get("utterance_id")})}else{json!({"status":"rejected","error":"room.binding_foreign","event_id":p.get("event_id")})}};
        let mut speech=p.clone();speech["thread_id"]=json!(thread);self.publish(&speech,v3)
    }
    pub fn receipt(&self,sid:&str,uid:&str,revision:u64,status:&str)->Result<Value,RoomError>{let mut inner=self.inner.lock().expect("room lock");
        if !["playing","failed","playback_finished","skipped","cancelled_unplayed","cancelled_playing"].contains(&status){return Err(RoomError::new(400,"room.receipt_invalid"));}
        if inner.browsers.get(sid).is_none_or(|c|c.revision!=revision){return Err(RoomError::new(409,"room.stale_utterance"));}
        let Some(record)=inner.utterances.get_mut(uid) else{return Err(RoomError::new(409,"room.stale_utterance"));};
        let Some(entry)=record.clients.get_mut(sid) else{return Err(RoomError::new(409,"room.stale_utterance"));};
        if entry.0!=revision||["failed","playback_finished","skipped","cancelled_unplayed","cancelled_playing"].contains(&entry.1.as_str()){return Err(RoomError::new(409,"room.stale_utterance"));}
        entry.1=status.into();let row_id=record.row_id.clone();
        if let Some(c)=inner.browsers.get_mut(sid){c.active=Some(uid.into());}
        if let Some(row)=inner.rows.iter_mut().find(|r|r.id==row_id){row.status=status.into();}
        Ok(json!({"status":status}))}
    pub fn working(&self,cid:&str,data:&Value){let bid=field(data,"binding_id");let mut inner=self.inner.lock().expect("room lock");let Some(b)=inner.bindings.get(bid).filter(|b|b.connector==cid&&b.live) else{return;};let thread=b.thread.clone();let Some(working)=data.get("working").and_then(Value::as_bool) else{return;};inner.working.insert(thread.clone(),working);
        for c in inner.browsers.values().filter(|c|c.target.as_ref().is_some_and(|t|t.thread==thread)){let mut out=json!({"thread_id":thread,"working":working});if let Some(obj)=out.as_object_mut(){for key in ["turn_id","turn_phase","session_id","revision"]{if let Some(v)=data.get(key){obj.insert(key.into(),v.clone());}}}let _=c.sender.try_send(json!({"type":"voice-conversation","data":out}));}}
    pub fn engine(&self,cid:&str,data:&Value){let mut inner=self.inner.lock().expect("room lock");if let Some(b)=inner.bindings.get_mut(field(data,"binding_id")).filter(|b|b.connector==cid&&b.live){if let Some(e)=engine(data.get("engine")){if e.get("model").is_some(){b.engine=Some(e);}}}}
    pub fn read(&self,cid:&str,data:&Value){let mut inner=self.inner.lock().expect("room lock");let Some(b)=inner.bindings.get(field(data,"binding_id")).filter(|b|b.connector==cid&&b.live)else{return;};let thread=b.thread.clone();let mid=field(data,"message_id");
        if let Some(row)=inner.rows.iter_mut().rev().find(|r|r.thread==thread&&r.role=="user"&&r.payload.as_ref().is_some_and(|p|field(p,"message_id")==mid)&&!matches!(r.status.as_str(),"read"|"not_sent")){row.status="read".into();let sid=row.session.clone();let payload=row.payload.clone().unwrap_or_default();if let Some(c)=inner.browsers.get(&sid){let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":payload["history_id"],"thread_id":thread,"session_id":sid,"status":"read"}}));}}}
    pub fn pending_delivery(&self)->Vec<(String,String,ConnectorPeer,Value)>{let mut inner=self.inner.lock().expect("room lock");let now=seconds();let mut work=Vec::new();
        for ix in 0..inner.rows.len(){let row=&inner.rows[ix];if row.role!="user"||row.status!="pending"{continue;}let rid=row.id.clone();let thread=row.thread.clone();let queued=row.queued_at;let next=row.next_attempt;
            if now.saturating_sub(queued)>=INPUT_TTL {
                inner.rows[ix].status="not_sent".into();inner.rows[ix].reason=Some("expired".into());
                let payload=inner.rows[ix].payload.clone().unwrap_or_default();let sid=inner.rows[ix].session.clone();
                if let Some(c)=inner.browsers.get(&sid){let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":rid,"thread_id":thread,"session_id":sid,"status":"not_sent"}}));}
                continue;
            }if next>now {continue;}
            let Some(b)=inner.bindings.values().filter(|b|b.active&&b.live&&b.thread==thread&&!inner.inflight.contains_key(&b.id)).max_by_key(|b|b.created) else{continue;};
            let bid=b.id.clone();let Some(peer)=inner.peers.get(&b.connector).cloned()else{continue;};let row=&inner.rows[ix];let payload=row.payload.as_ref().unwrap();
            let data=json!({"event_id":rid,"binding_id":bid,"thread":thread,"text":row.text,"channel":"voice","session_id":payload["session_id"],"revision":payload["revision"],"message_id":payload["message_id"]});
            inner.rows[ix].status="sending".into();inner.inflight.insert(bid.clone(),rid.clone());work.push((bid,rid,peer,data));
        }work}
    pub fn settle_delivery(&self,bid:&str,rid:&str,generation:&str,answer:Result<Value,PeerError>){let mut inner=self.inner.lock().expect("room lock");if inner.inflight.get(bid).is_none_or(|id|id!=rid){return;}let Some(b)=inner.bindings.get(bid)else{return;};if inner.peers.get(&b.connector).is_none_or(|p|p.generation!=generation){return;}inner.inflight.remove(bid);
        let Some(row)=inner.rows.iter_mut().find(|r|r.id==rid)else{return;};if row.status=="read"{return;}let status=answer.as_ref().ok().and_then(valid_delivery_ack);
        let new_status=match status{Some("accepted")=>"delivered",Some("unknown")=>"unconfirmed",Some("unsupported")=>"not_sent",_=>"pending"};row.status=new_status.into();
        row.reason=match status{Some("unknown")=>answer.as_ref().ok().and_then(|v|v.get("detail")).and_then(Value::as_str).map(str::to_owned),Some("unsupported")=>Some("unsupported".into()),_=>None};
        if new_status=="pending"{row.attempts+=1;row.next_attempt=seconds()+[2,5,15,60][row.attempts.min(4)-1];}
        let sid=row.session.clone();let payload=row.payload.clone().unwrap_or_default();if let Some(c)=inner.browsers.get_mut(&sid){if new_status=="delivered"{c.sent+=1;}let _=c.sender.try_send(json!({"type":"voice-input-receipt","data":{"revision":payload["revision"],"history_id":rid,"thread_id":payload["thread_id"],"session_id":sid,"status":new_status}}));}}
    pub async fn pump(self: std::sync::Arc<Self>) {loop{for (bid,rid,peer,data) in self.pending_delivery(){let room=self.clone();tokio::spawn(async move{let answer=peer.request("input.deliver",data,ACK_TIMEOUT).await;room.settle_delivery(&bid,&rid,&peer.generation,answer);});}tokio::time::sleep(Duration::from_millis(250)).await;}}
}
fn trim_rows(inner:&mut Inner){while inner.rows.len()>MAX_HISTORY&&inner.rows.front().is_some_and(|r|!matches!(r.status.as_str(),"pending"|"sending")){inner.rows.pop_front();}}
fn engine(value:Option<&Value>)->Option<Value>{let obj=value?.as_object()?;let mut out=Map::new();for key in ["model","effort","thinking"]{if let Some(v)=obj.get(key).filter(|v|!v.is_null()&&v.as_str()!=Some("")){out.insert(key.into(),json!(v.as_str().map(str::to_owned).unwrap_or_else(||v.to_string()).chars().take(60).collect::<String>()));}}(!out.is_empty()).then_some(Value::Object(out))}
fn capabilities(value:Option<&Value>,experimental:Option<&Value>)->Value{let mut result=Map::new();for key in ["deliver","inspectInbound","working","endOfTurn","sessionIdentity"]{let status=value.and_then(|v|v.get(key)).and_then(Value::as_str).filter(|s|["supported","unsupported"].contains(s)).unwrap_or("unknown");result.insert(key.into(),json!(status));}
    let marked=experimental.or_else(||value.and_then(|v|v.get("experimental"))).and_then(Value::as_array);if let Some(marked)=marked{let names:Vec<&str>=["deliver","inspectInbound","working","endOfTurn","sessionIdentity"].into_iter().filter(|key|result[*key]=="supported"&&marked.iter().any(|v|v.as_str()==Some(key))).collect();if !names.is_empty(){result.insert("experimental".into(),json!(names));}}Value::Object(result)}
