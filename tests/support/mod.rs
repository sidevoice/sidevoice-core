//! What every integration test drives the core with: the real binary as a process, HTTP over its TCP port or its
//! local socket, a browser's call socket, and the peers on the other end of its links (a connector over Socket.IO
//! v2 or JSON-RPC v3, the room over Socket.IO), each written here against the wire. Nothing from another
//! repository, no Docker.
#![allow(dead_code)]

pub mod webrtc_peer;

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const CORE: &str = env!("CARGO_BIN_EXE_sidevoice-core-rust");

/// How long a step may take before the test says which one did not happen.
pub const STEP: Duration = Duration::from_secs(10);

/// A recorded voice: "Hola, esto es una prueba de voz de la sala.", 16 kHz mono (tests/fixtures/README.md).
pub fn speech() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hola-sala-16k.wav");
    let mut reader = hound::WavReader::open(&path).expect("the speech fixture opens");
    let spec = reader.spec();
    assert_eq!((spec.sample_rate, spec.channels), (16_000, 1));
    reader
        .samples::<i16>()
        .flat_map(|sample| sample.expect("a 16-bit sample").to_le_bytes())
        .collect()
}

/// `seconds` of 16 kHz mono silence.
pub fn silence(seconds: f32) -> Vec<u8> {
    vec![0; (16_000.0 * seconds) as usize * 2]
}

/// One test at a time for the tests that run the voice detectors in real time: two of them side by side on a
/// small runner measure the scheduler, not the core.
pub async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

pub fn until<T>(within: Duration, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub async fn eventually<T, F>(within: Duration, what: &str, mut check: impl FnMut() -> F) -> T
where
    F: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The core as a process.

/// A start of the core binary: its data directory, arguments and environment. The environment is the test's own
/// minus everything that would reach a real provider, a collector or a public STUN server.
pub struct Launch {
    data: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl Launch {
    pub fn new(data: impl Into<PathBuf>) -> Self {
        Self {
            data: data.into(),
            args: Vec::new(),
            // English messages, and no public STUN server: a test reaches nothing outside this machine.
            env: vec![
                ("SIDEVOICE_STUN_URLS".into(), String::new()),
                ("LC_ALL".into(), "en_US.UTF-8".into()),
            ],
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, name: &str, value: impl Into<String>) -> Self {
        self.env.retain(|(known, _)| known != name);
        self.env.push((name.into(), value.into()));
        self
    }

    pub fn spawn(self) -> Core {
        let stderr = self.data.with_extension(format!(
            "stderr-{}.log",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if let Some(parent) = stderr.parent() {
            fs::create_dir_all(parent).expect("the test root exists");
        }
        let mut command = Command::new(CORE);
        command
            .arg("--data-dir")
            .arg(&self.data)
            .args(["--port", "0", "--idle-exit", "0"])
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&stderr).expect("stderr file"));
        for name in [
            "VOICE_ELEVENLABS_API_KEY",
            "VOICE_STT_API_KEY",
            "OPENAI_API_KEY",
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "VOICE_PUBLIC_ORIGIN",
            "SIDEVOICE_ALLOWED_ORIGINS",
            "SIDEVOICE_ALLOWED_HOSTS",
        ] {
            command.env_remove(name);
        }
        for (name, value) in &self.env {
            command.env(name, value);
        }
        Core {
            child: command.spawn().expect("the core binary starts"),
            data: self.data,
            stderr,
            ready: Value::Null,
            port: 0,
        }
    }

    /// Starts the core and waits for the ready file it writes for this process.
    pub fn start(self) -> Core {
        let mut core = self.spawn();
        core.wait_ready();
        core
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct Core {
    child: Child,
    pub data: PathBuf,
    stderr: PathBuf,
    pub ready: Value,
    pub port: u16,
}

impl Core {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn socket(&self) -> PathBuf {
        self.data.join("local.sock")
    }

    pub fn connector_id(&self) -> String {
        self.ready["connector_id"].as_str().unwrap().to_owned()
    }

    pub fn connector_token(&self) -> String {
        self.ready["token"].as_str().unwrap().to_owned()
    }

    fn wait_ready(&mut self) {
        let path = self.data.join("core.json");
        let pid = self.pid();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "the core exited before it was ready ({status}): {}",
                    self.log()
                );
            }
            let ready = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .filter(|ready| ready["pid"] == pid);
            if let Some(ready) = ready {
                self.port = ready["port"].as_u64().expect("a port") as u16;
                self.ready = ready;
                return;
            }
            assert!(Instant::now() < deadline, "no ready file: {}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn log(&self) -> String {
        let mut text = String::new();
        if let Ok(mut file) = fs::File::open(&self.stderr) {
            let _ = file.read_to_string(&mut text);
        }
        let tail: Vec<&str> = text.lines().rev().take(40).collect();
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    pub fn signal(&self, signal: libc::c_int) {
        unsafe {
            libc::kill(self.pid() as libc::pid_t, signal);
        }
    }

    /// The exit status, waiting at most `within`.
    pub fn exited(&mut self, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status.code().unwrap_or(-1));
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGTERM, the way a service manager stops it; its exit status.
    pub fn stop(&mut self) -> i32 {
        self.signal(libc::SIGTERM);
        self.exited(Duration::from_secs(15))
            .unwrap_or_else(|| panic!("the core ignored SIGTERM: {}", self.log()))
    }

    pub fn http(&self, method: &str, path: &str) -> Http {
        Http::new(Target::Tcp(self.port), method, path)
    }

    pub fn get(&self, path: &str) -> Http {
        self.http("GET", path)
    }

    pub fn post(&self, path: &str, body: Value) -> Http {
        self.http("POST", path).json(body)
    }

    /// A request over the local socket, which only this machine's user can open.
    pub fn local(&self, method: &str, path: &str) -> Http {
        Http::new(Target::Unix(self.socket()), method, path)
    }

    /// Pairs a device over the local socket, as the desktop app does; its bearer token.
    pub async fn pair_local(&self, name: &str) -> String {
        let reply = self
            .local("POST", "/api/device/local/pair")
            .json(json!({"name": name}))
            .send()
            .await;
        assert_eq!(reply.status, 200, "{reply:?}");
        reply.json()["token"].as_str().unwrap().to_owned()
    }

    /// Waits until the core counts `expected` open call sockets.
    pub async fn calls_become(&self, expected: u64) {
        let deadline = Instant::now() + STEP;
        loop {
            let open = self.calls().await;
            if open == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{open} calls open, not {expected}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The number of call sockets the core counts as open.
    pub async fn calls(&self) -> u64 {
        self.local("GET", "/api/local/health").send().await.json()["calls"]
            .as_u64()
            .unwrap()
    }

    pub async fn revision(&self, token: &str, session: &str) -> u64 {
        self.get(&format!("/api/presentation?session_id={session}"))
            .token(token)
            .send()
            .await
            .json()["room"]["revision"]
            .as_u64()
            .expect("a room revision")
    }

    /// Focuses `thread` for `session`; the binding it now talks to.
    pub async fn select(&self, token: &str, session: &str, thread: &str) -> String {
        let reply = self
            .post(
                "/api/presentation/select",
                json!({"session_id": session, "thread_id": thread}),
            )
            .token(token)
            .send()
            .await;
        assert_eq!(reply.status, 200, "select {thread}: {reply:?}");
        reply.json()["binding"]["binding_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Typed input to the focused conversation (session, thread, binding), under the browser's own
    /// `message_id`; the answer (`id` is its history row).
    pub async fn text(
        &self,
        token: &str,
        focus: (&str, &str, &str),
        text: &str,
        message_id: &str,
    ) -> Value {
        let (session, thread, binding) = focus;
        let reply = self
            .post(
                "/api/presentation/text",
                json!({"text": text, "session_id": session, "thread_id": thread,
                    "binding_id": binding, "message_id": message_id}),
            )
            .token(token)
            .send()
            .await;
        assert_eq!(reply.status, 200, "text: {reply:?}");
        reply.json()
    }

    pub async fn receipt(
        &self,
        token: &str,
        session: &str,
        utterance: &str,
        revision: u64,
        status: &str,
    ) -> u16 {
        self.post(
            "/api/presentation/browser-receipt",
            json!({"session_id": session, "utterance_id": utterance, "revision": revision, "status": status}),
        )
        .token(token)
        .send()
        .await
        .status
    }

    /// Plays a reply to its end, as a browser reports it.
    pub async fn played(&self, token: &str, session: &str, utterance: &str, revision: u64) {
        for status in ["playing", "playback_finished"] {
            assert_eq!(
                self.receipt(token, session, utterance, revision, status)
                    .await,
                200,
                "{status} receipt for {utterance}"
            );
        }
    }

    pub async fn history(&self, token: &str, thread: &str) -> Vec<Value> {
        self.get(&format!("/api/presentation/history?thread_id={thread}"))
            .token(token)
            .send()
            .await
            .json()["messages"]
            .as_array()
            .expect("history messages")
            .clone()
    }

    pub async fn participant(&self, token: &str, thread: &str) -> Value {
        self.get("/api/presentation/participants")
            .token(token)
            .send()
            .await
            .json()["participants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["thread_id"] == thread)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// A browser's call socket, opened with the token as a subprotocol, as the web client does.
    pub async fn open_call(&self, token: &str) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
        let mut request = format!("ws://127.0.0.1:{}/api/presentation/ws", self.port)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            format!("sidevoice, sidevoice.token.{token}")
                .parse()
                .unwrap(),
        );
        tokio_tungstenite::connect_async(request)
            .await
            .expect("the call socket opens")
            .0
    }

    /// A browser in a call: hello sent with these settings and the test device's models, session assigned.
    pub async fn join(&self, token: &str, settings: Value) -> Browser {
        let mut browser = Browser::new(self.open_call(token).await);
        let hello = if settings.is_null() {
            json!({"device_models": device_models()})
        } else {
            json!({"settings": settings, "device_models": device_models()})
        };
        browser.send("voice-hello", hello).await;
        let session = browser.frame("voice-session").await;
        browser.session = session["session_id"].as_str().unwrap().to_owned();
        browser.welcome = session;
        browser
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.signal(libc::SIGTERM);
            if self.exited(Duration::from_secs(10)).is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        if std::thread::panicking() {
            eprintln!("--- core stderr ---\n{}", self.log());
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// HTTP/1.1, one exchange per connection, over TCP or the local socket.

#[derive(Clone)]
enum Target {
    Tcp(u16),
    Unix(PathBuf),
}

pub struct Http {
    target: Target,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout: Duration,
}

#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| {
            panic!(
                "a JSON body, not {:?} ({})",
                String::from_utf8_lossy(&self.body),
                self.status
            )
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The `detail.key` a refusal carries.
    pub fn key(&self) -> String {
        self.json()["detail"]["key"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
}

impl Http {
    fn new(target: Target, method: &str, path: &str) -> Self {
        Self {
            target,
            method: method.into(),
            path: path.into(),
            headers: vec![("host".into(), "localhost".into())],
            body: None,
            timeout: Duration::from_secs(60),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .retain(|(known, _)| !known.eq_ignore_ascii_case(name));
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn token(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    pub fn origin(self, origin: &str) -> Self {
        self.header("origin", origin)
    }

    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(serde_json::to_vec(&body).unwrap());
        self.header("content-type", "application/json")
    }

    pub async fn send(self) -> Reply {
        let what = format!("{} {}", self.method, self.path);
        let answer = async {
            match &self.target {
                Target::Tcp(port) => {
                    exchange(
                        TcpStream::connect(("127.0.0.1", *port)).await.unwrap(),
                        &self,
                    )
                    .await
                }
                Target::Unix(path) => {
                    exchange(UnixStream::connect(path).await.unwrap(), &self).await
                }
            }
        };
        tokio::time::timeout(self.timeout, answer)
            .await
            .unwrap_or_else(|_| panic!("{what} got no answer"))
    }
}

async fn exchange(mut stream: impl AsyncRead + AsyncWrite + Unpin, request: &Http) -> Reply {
    let mut head = format!("{} {} HTTP/1.1\r\n", request.method, request.path);
    for (name, value) in &request.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = &request.body {
        head.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    head.push_str("connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    if let Some(body) = &request.body {
        stream.write_all(body).await.unwrap();
    }
    stream.flush().await.unwrap();
    let mut raw = Vec::new();
    // A server that closes right after answering may reset the connection (macOS does, after an `Upgrade`
    // request it refused); what it answered is already read.
    if let Err(error) = stream.read_to_end(&mut raw).await {
        assert!(
            error.kind() == std::io::ErrorKind::ConnectionReset && !raw.is_empty(),
            "{error}"
        );
    }
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("an HTTP head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    if headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"))
    {
        body = dechunk(&body);
    }
    Reply {
        status,
        headers,
        body,
    }
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let Some(end) = raw.windows(2).position(|window| window == b"\r\n") else {
            return body;
        };
        let size = std::str::from_utf8(&raw[..end])
            .ok()
            .and_then(|line| usize::from_str_radix(line.split(';').next()?.trim(), 16).ok())
            .unwrap_or(0);
        if size == 0 {
            return body;
        }
        body.extend_from_slice(&raw[end + 2..end + 2 + size]);
        raw = &raw[end + 2 + size + 2..];
    }
}

// ---------------------------------------------------------------------------------------------------------------
// WebSocket plumbing shared by the browser and the peers: one writer task, reads where the test reads.

type Writer = mpsc::UnboundedSender<Message>;

fn writer<S>(sink: SplitSink<WebSocketStream<S>, Message>) -> Writer
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let mut sink = sink;
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let close = matches!(message, Message::Close(_));
            if sink.send(message).await.is_err() || close {
                break;
            }
        }
        let _ = sink.close().await;
    });
    tx
}

fn close_code(message: &Message) -> Option<u16> {
    match message {
        Message::Close(Some(frame)) => Some(u16::from(frame.code)),
        Message::Close(None) => Some(1005),
        _ => None,
    }
}

/// Sends PCM the way a microphone does: 20 ms (640 bytes) at a time, in real time.
#[derive(Clone)]
pub struct Microphone(Writer);

impl Microphone {
    pub async fn speak(&self, pcm: &[u8]) {
        let started = Instant::now();
        for (index, chunk) in pcm.chunks(640).enumerate() {
            if self.0.send(Message::binary(chunk.to_vec())).is_err() {
                return;
            }
            let due = started + Duration::from_millis(20 * (index as u64 + 1));
            tokio::time::sleep_until(due.into()).await;
        }
    }
}

/// An event as a failure message shows it: no audio, nothing long.
fn summary(event: &Value) -> String {
    let mut event = event.clone();
    if let Some(data) = event["data"].as_object_mut() {
        data.retain(|key, _| !key.contains("audio"));
    }
    let text = event.to_string();
    if text.len() > 300 {
        format!("{}…", &text[..text.floor_char_boundary(300)])
    } else {
        text
    }
}

/// A browser's end of a call socket.
pub struct Browser {
    tx: Writer,
    rx: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    pub session: String,
    pub welcome: Value,
    pub close_code: Option<u16>,
}

impl Browser {
    pub fn new(ws: WebSocketStream<MaybeTlsStream<TcpStream>>) -> Self {
        let (sink, rx) = ws.split();
        Self {
            tx: writer(sink),
            rx,
            session: String::new(),
            welcome: Value::Null,
            close_code: None,
        }
    }

    pub fn microphone(&self) -> Microphone {
        Microphone(self.tx.clone())
    }

    pub async fn send(&self, kind: &str, data: Value) {
        let _ = self.tx.send(Message::text(
            json!({"type": kind, "data": data}).to_string(),
        ));
    }

    pub fn send_raw(&self, message: Message) {
        let _ = self.tx.send(message);
    }

    pub async fn speak(&self, pcm: &[u8]) {
        self.microphone().speak(pcm).await;
    }

    pub async fn transcript(&self, ask: &Value, text: &str) {
        self.send(
            "voice-transcript",
            json!({"session_id": self.session, "request_id": ask["request_id"], "text": text}),
        )
        .await;
    }

    /// The next event the core sends, or `None` once `within` passes or the socket closes.
    pub async fn next(&mut self, within: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let message = tokio::time::timeout_at(deadline, self.rx.next())
                .await
                .ok()??;
            let Ok(message) = message else {
                return None;
            };
            if let Some(code) = close_code(&message) {
                self.close_code = Some(code);
                return None;
            }
            let Message::Text(text) = message else {
                continue;
            };
            let event: Value = serde_json::from_str(text.as_str()).expect("a JSON event");
            if event["type"] == "voice-ping" {
                self.send("voice-pong", json!({"session_id": self.session}))
                    .await;
                continue;
            }
            return Some(event);
        }
    }

    /// Waits for an event of `kind` that `accept` takes, discarding the others; its `data`.
    pub async fn wait(
        &mut self,
        kind: &str,
        within: Duration,
        accept: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = tokio::time::Instant::now() + within;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(event) = self.next(left).await else {
                panic!(
                    "no {kind} within {within:?} (closed: {:?}); seen: {seen:?}",
                    self.close_code
                );
            };
            if event["type"] == kind && accept(&event["data"]) {
                return event["data"].clone();
            }
            seen.push(summary(&event));
        }
    }

    pub async fn frame(&mut self, kind: &str) -> Value {
        self.wait(kind, STEP, |_| true).await
    }

    pub async fn frame_within(&mut self, kind: &str, within: Duration) -> Value {
        self.wait(kind, within, |_| true).await
    }

    pub async fn receipt(&mut self, status: &str) -> Value {
        self.wait("voice-input-receipt", STEP, |data| data["status"] == status)
            .await
    }

    /// The turn that reaches `phase`, skipping the turn events before it.
    pub async fn turn(&mut self, phase: &str, within: Duration) -> Value {
        self.wait("voice-user-turn", within, |data| data["phase"] == phase)
            .await
    }

    /// Nothing of these kinds arrives for `within`.
    pub async fn none_of(&mut self, kinds: &[&str], within: Duration) {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(event) = self.next(left).await else {
                return;
            };
            let kind = event["type"].as_str().unwrap_or("");
            assert!(
                !kinds.contains(&kind),
                "unexpected {kind}: {}",
                summary(&event)
            );
        }
    }

    /// Nothing at all arrives for `within`.
    pub async fn quiet(&mut self, within: Duration) {
        if let Some(event) = self.next(within).await {
            panic!("unexpected event: {}", summary(&event));
        }
    }

    /// The close code the core ends this socket with.
    pub async fn closed(&mut self, within: Duration) -> u16 {
        let deadline = tokio::time::Instant::now() + within;
        while self.close_code.is_none() {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!left.is_zero(), "the core kept the socket open");
            if self.next(left).await.is_none() && self.close_code.is_none() {
                return 1006;
            }
        }
        self.close_code.unwrap()
    }

    pub async fn close(self) {
        let _ = self.tx.send(Message::Close(None));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Connector link v3: JSON-RPC 2.0 over a WebSocket on the local socket.

pub struct Rpc {
    tx: Writer,
    rx: SplitStream<WebSocketStream<UnixStream>>,
    seen: Vec<Value>,
    next_id: i64,
    pub close_code: Option<u16>,
}

impl Rpc {
    pub async fn open(core: &Core) -> Self {
        let stream = UnixStream::connect(core.socket()).await.unwrap();
        let (ws, _) = tokio_tungstenite::client_async("ws://localhost/api/connectors/v3", stream)
            .await
            .expect("the v3 link opens over the local socket");
        let (sink, rx) = ws.split();
        Self {
            tx: writer(sink),
            rx,
            seen: Vec::new(),
            next_id: 100,
            close_code: None,
        }
    }

    /// Opens the link and says hello with the core's own connector credential.
    pub async fn hello(core: &Core) -> Self {
        let mut rpc = Self::open(core).await;
        let welcome = rpc
            .request(
                "connector.hello",
                json!({"protocol": 3, "connector_id": core.connector_id(), "token": core.connector_token()}),
            )
            .await;
        assert_eq!(welcome["result"]["protocol"], 3, "{welcome}");
        rpc
    }

    pub fn send(&self, frame: Value) {
        let _ = self.tx.send(Message::text(frame.to_string()));
    }

    pub fn send_text(&self, text: &str) {
        let _ = self.tx.send(Message::text(text.to_owned()));
    }

    async fn read(&mut self, within: Duration) -> Option<Value> {
        let message = tokio::time::timeout(within, self.rx.next()).await.ok()??;
        let message = message.ok()?;
        if let Some(code) = close_code(&message) {
            self.close_code = Some(code);
            return None;
        }
        match message {
            Message::Text(text) => Some(serde_json::from_str(text.as_str()).expect("JSON-RPC")),
            _ => Some(Value::Null),
        }
    }

    /// The next frame `accept` takes, keeping the others for later waits.
    pub async fn frame(&mut self, within: Duration, accept: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.seen.iter().position(&accept) {
            return self.seen.remove(index);
        }
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(frame) = self.read(left).await else {
                panic!(
                    "no matching JSON-RPC frame (closed: {:?}); seen: {:?}",
                    self.close_code, self.seen
                );
            };
            if accept(&frame) {
                return frame;
            }
            self.seen.push(frame);
        }
    }

    /// A request from the core.
    pub async fn method(&mut self, name: &str, within: Duration) -> Value {
        self.frame(within, |frame| frame["method"] == name).await
    }

    /// A request to the core; its whole response frame.
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.frame(STEP, |frame| {
            frame["id"] == id && frame.get("method").is_none()
        })
        .await
    }

    pub fn answer(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    pub async fn closed(&mut self, within: Duration) -> u16 {
        let deadline = tokio::time::Instant::now() + within;
        while self.close_code.is_none() {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!left.is_zero(), "the core kept the v3 link open");
            if self.read(left).await.is_none() && self.close_code.is_none() {
                return 1006;
            }
        }
        self.close_code.unwrap()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Socket.IO v5 over Engine.IO v4, WebSocket transport only: enough of either end for the core's three links.

#[derive(Debug, Clone)]
pub struct SioEvent {
    pub name: String,
    pub data: Value,
    pub id: Option<u64>,
    pub attachments: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct SioAck {
    pub data: Value,
    pub attachments: Vec<Vec<u8>>,
}

impl SioAck {
    /// The bytes a `{"_placeholder": true, "num": N}` in this answer stands for.
    pub fn binary(&self, placeholder: &Value) -> Vec<u8> {
        assert_eq!(
            placeholder["_placeholder"], true,
            "not a binary attachment: {placeholder}"
        );
        self.attachments[placeholder["num"].as_u64().unwrap() as usize].clone()
    }
}

/// One Socket.IO packet as it travels inside an Engine.IO message.
struct Packet {
    kind: u8,
    attachments: usize,
    namespace: String,
    id: Option<u64>,
    data: Option<Value>,
}

fn parse_packet(text: &str) -> Option<Packet> {
    let kind = text.bytes().next()?.checked_sub(b'0')?;
    let mut rest = &text[1..];
    let mut attachments = 0;
    if kind == 5 || kind == 6 {
        let dash = rest.find('-')?;
        attachments = rest[..dash].parse().ok()?;
        rest = &rest[dash + 1..];
    }
    let mut namespace = "/".to_owned();
    if rest.starts_with('/') {
        let end = rest.find(',').unwrap_or(rest.len());
        namespace = rest[..end].to_owned();
        rest = rest.get(end + 1..).unwrap_or("");
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let id = (digits > 0).then(|| rest[..digits].parse().ok()).flatten();
    rest = &rest[digits..];
    let data = (!rest.is_empty())
        .then(|| serde_json::from_str(rest).ok())
        .flatten();
    Some(Packet {
        kind,
        attachments,
        namespace,
        id,
        data,
    })
}

fn encode_packet(
    kind: u8,
    attachments: usize,
    namespace: &str,
    id: Option<u64>,
    data: Option<&Value>,
) -> String {
    let mut text = format!("4{kind}");
    if attachments > 0 {
        text.push_str(&format!("{attachments}-"));
    }
    if namespace != "/" {
        text.push_str(namespace);
        text.push(',');
    }
    if let Some(id) = id {
        text.push_str(&id.to_string());
    }
    if let Some(data) = data {
        text.push_str(&data.to_string());
    }
    text
}

type Acks = Arc<Mutex<HashMap<u64, oneshot::Sender<SioAck>>>>;

/// Either end of a Socket.IO connection on one namespace.
pub struct Sio {
    tx: Writer,
    namespace: String,
    events: mpsc::UnboundedReceiver<SioEvent>,
    seen: Vec<SioEvent>,
    acks: Acks,
    next_id: AtomicU64,
    gone: watch::Receiver<bool>,
}

/// What the reader reports before any event: the peer's CONNECT (its auth, or the server's answer) or its refusal.
enum Opening {
    Connect(Value),
    Refused(Value),
}

impl Sio {
    fn spawn<S>(
        ws: WebSocketStream<S>,
        namespace: &str,
        client: bool,
    ) -> (Self, oneshot::Receiver<Opening>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, mut rx) = ws.split();
        let tx = writer(sink);
        let (events_tx, events) = mpsc::unbounded_channel();
        let acks: Acks = Arc::default();
        let (opened_tx, opened) = oneshot::channel();
        let (gone_tx, gone) = watch::channel(false);
        let reader_tx = tx.clone();
        let reader_acks = acks.clone();
        tokio::spawn(async move {
            let mut opened_tx = Some(opened_tx);
            let mut partial: Option<(Packet, Vec<Vec<u8>>)> = None;
            while let Some(Ok(message)) = rx.next().await {
                let packet = match message {
                    Message::Text(text) => {
                        let text = text.as_str();
                        match text.as_bytes().first() {
                            // Engine.IO ping from a server: a client answers it.
                            Some(b'2') if client => {
                                let _ = reader_tx.send(Message::text("3"));
                                continue;
                            }
                            Some(b'4') => match parse_packet(&text[1..]) {
                                Some(packet) if packet.attachments > 0 => {
                                    partial = Some((packet, Vec::new()));
                                    continue;
                                }
                                Some(packet) => (packet, Vec::new()),
                                None => continue,
                            },
                            Some(b'1') => break,
                            _ => continue,
                        }
                    }
                    Message::Binary(bytes) => {
                        let Some((packet, mut attachments)) = partial.take() else {
                            continue;
                        };
                        attachments.push(bytes.to_vec());
                        if attachments.len() < packet.attachments {
                            partial = Some((packet, attachments));
                            continue;
                        }
                        (packet, attachments)
                    }
                    Message::Close(_) => break,
                    _ => continue,
                };
                let (packet, attachments) = packet;
                match packet.kind {
                    0 => {
                        if let Some(opened) = opened_tx.take() {
                            let _ =
                                opened.send(Opening::Connect(packet.data.unwrap_or(Value::Null)));
                        }
                    }
                    4 => {
                        if let Some(opened) = opened_tx.take() {
                            let _ =
                                opened.send(Opening::Refused(packet.data.unwrap_or(Value::Null)));
                        }
                        break;
                    }
                    1 => break,
                    2 | 5 => {
                        let args = packet
                            .data
                            .and_then(|data| data.as_array().cloned())
                            .unwrap_or_default();
                        let name = args
                            .first()
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let data = args.get(1).cloned().unwrap_or(Value::Null);
                        let _ = events_tx.send(SioEvent {
                            name,
                            data,
                            id: packet.id,
                            attachments,
                        });
                    }
                    3 | 6 => {
                        let data = packet
                            .data
                            .and_then(|data| data.as_array().and_then(|args| args.first().cloned()))
                            .unwrap_or(Value::Null);
                        let waiter = packet
                            .id
                            .and_then(|id| reader_acks.lock().unwrap().remove(&id));
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(SioAck { data, attachments });
                        }
                    }
                    _ => {}
                }
            }
            let _ = gone_tx.send(true);
        });
        (
            Self {
                tx,
                namespace: namespace.to_owned(),
                events,
                seen: Vec::new(),
                acks,
                next_id: AtomicU64::new(1),
                gone,
            },
            opened,
        )
    }

    /// Connects to `namespace` as a client; the server's refusal (CONNECT_ERROR data) if it refuses.
    pub async fn client<S>(
        ws: WebSocketStream<S>,
        namespace: &str,
        auth: Value,
    ) -> Result<Self, Value>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sio, opened) = Self::spawn(ws, namespace, true);
        let _ = sio.tx.send(Message::text(encode_packet(
            0,
            0,
            namespace,
            None,
            Some(&auth),
        )));
        match tokio::time::timeout(STEP, opened).await {
            Ok(Ok(Opening::Connect(_))) => Ok(sio),
            Ok(Ok(Opening::Refused(data))) => Err(data),
            _ => Err(Value::Null),
        }
    }

    /// The server's side of a connection a client opened: Engine.IO open packet, then the client's CONNECT to
    /// `namespace`, accepted. The client's auth.
    pub async fn server<S>(ws: WebSocketStream<S>, namespace: &str) -> (Self, Value)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sio, opened) = Self::spawn(ws, namespace, false);
        // Pings are left out: an interval longer than any test keeps the client from expecting one.
        let open = json!({"sid": uuid::Uuid::new_v4().to_string(), "upgrades": [],
            "pingInterval": 600_000, "pingTimeout": 600_000, "maxPayload": 100_000_000});
        let _ = sio.tx.send(Message::text(format!("0{open}")));
        let Ok(Ok(Opening::Connect(auth))) = tokio::time::timeout(STEP, opened).await else {
            panic!("the client never connected to {namespace}");
        };
        let sid = json!({"sid": uuid::Uuid::new_v4().to_string()});
        let _ = sio.tx.send(Message::text(encode_packet(
            0,
            0,
            namespace,
            None,
            Some(&sid),
        )));
        (sio, auth)
    }

    pub fn emit(&self, name: &str, data: Value) {
        let _ = self.tx.send(Message::text(encode_packet(
            2,
            0,
            &self.namespace,
            None,
            Some(&json!([name, data])),
        )));
    }

    /// An event whose data holds `{"_placeholder": true, "num": N}` for each attachment, in order.
    pub fn emit_binary(&self, name: &str, data: Value, attachments: Vec<Vec<u8>>) {
        let _ = self.tx.send(Message::text(encode_packet(
            5,
            attachments.len(),
            &self.namespace,
            None,
            Some(&json!([name, data])),
        )));
        for attachment in attachments {
            let _ = self.tx.send(Message::binary(attachment));
        }
    }

    /// An event that asks for an acknowledgement; the answer, or `None` if none came within `within`.
    pub async fn call_within(
        &self,
        name: &str,
        data: Value,
        attachments: Vec<Vec<u8>>,
        within: Duration,
    ) -> Option<SioAck> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        self.acks.lock().unwrap().insert(id, answer);
        let kind = if attachments.is_empty() { 2 } else { 5 };
        let _ = self.tx.send(Message::text(encode_packet(
            kind,
            attachments.len(),
            &self.namespace,
            Some(id),
            Some(&json!([name, data])),
        )));
        for attachment in attachments {
            let _ = self.tx.send(Message::binary(attachment));
        }
        tokio::time::timeout(within, answered).await.ok()?.ok()
    }

    pub async fn call(&self, name: &str, data: Value) -> SioAck {
        self.call_within(name, data, Vec::new(), STEP)
            .await
            .unwrap_or_else(|| panic!("no answer to {name}"))
    }

    /// Acknowledges an event the other end sent with an id.
    pub fn answer(&self, id: u64, data: Value) {
        let _ = self.tx.send(Message::text(encode_packet(
            3,
            0,
            &self.namespace,
            Some(id),
            Some(&json!([data])),
        )));
    }

    /// The next event named `name`, keeping the others for later waits.
    pub async fn try_event(&mut self, name: &str, within: Duration) -> Option<SioEvent> {
        if let Some(index) = self.seen.iter().position(|event| event.name == name) {
            return Some(self.seen.remove(index));
        }
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .ok()??;
            if event.name == name {
                return Some(event);
            }
            self.seen.push(event);
        }
    }

    pub async fn event(&mut self, name: &str) -> SioEvent {
        self.event_within(name, STEP).await
    }

    pub async fn event_within(&mut self, name: &str, within: Duration) -> SioEvent {
        let seen: Vec<String> = self.seen.iter().map(|event| event.name.clone()).collect();
        self.try_event(name, within)
            .await
            .unwrap_or_else(|| panic!("no {name} event; held: {seen:?}"))
    }

    /// Every event received so far, without waiting.
    pub fn drain(&mut self) -> Vec<SioEvent> {
        while let Ok(event) = self.events.try_recv() {
            self.seen.push(event);
        }
        std::mem::take(&mut self.seen)
    }

    /// Leaves the namespace and closes the connection.
    pub fn disconnect(&self) {
        let _ = self.tx.send(Message::text(encode_packet(
            1,
            0,
            &self.namespace,
            None,
            None,
        )));
        let _ = self.tx.send(Message::Close(None));
    }

    pub fn is_gone(&self) -> bool {
        *self.gone.borrow()
    }

    /// Whether the other end ended the connection within `within`.
    pub async fn gone_within(&self, within: Duration) -> bool {
        let mut gone = self.gone.clone();
        let waited = tokio::time::timeout(within, gone.wait_for(|gone| *gone)).await;
        waited.is_ok()
    }
}

/// The connector's Socket.IO v2 link to the core, over the local socket.
pub async fn connector_v2(core: &Core, identity: Value) -> Sio {
    let stream = UnixStream::connect(core.socket()).await.unwrap();
    let (ws, _) = tokio_tungstenite::client_async(
        "ws://localhost/api/connectors/link/?EIO=4&transport=websocket",
        stream,
    )
    .await
    .expect("the v2 link opens over the local socket");
    let mut auth = json!({"connector_id": core.connector_id(), "token": core.connector_token(), "protocol": 2});
    if let (Some(auth), Some(identity)) = (auth.as_object_mut(), identity.as_object()) {
        auth.extend(identity.clone());
    }
    Sio::client(ws, "/connectors", auth)
        .await
        .expect("the core accepts its own connector credential")
}

/// A room listening for the core's outbound Socket.IO link: every connection it accepts, with the client's auth
/// and the path it asked for.
pub struct Room {
    pub port: u16,
    accepted: mpsc::UnboundedReceiver<(Sio, Value, String)>,
}

impl Room {
    pub async fn listen(namespace: &'static str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, accepted) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let path = Arc::new(Mutex::new(String::new()));
                    let seen = path.clone();
                    // The error type is tungstenite's own, whatever its size.
                    #[allow(clippy::result_large_err)]
                    let callback = move |request: &Request,
                                         response: Response|
                          -> Result<Response, ErrorResponse> {
                        *seen.lock().unwrap() = request.uri().to_string();
                        Ok(response)
                    };
                    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                        return;
                    };
                    let (sio, auth) = Sio::server(ws, namespace).await;
                    let path = path.lock().unwrap().clone();
                    let _ = tx.send((sio, auth, path));
                });
            }
        });
        Self { port, accepted }
    }

    pub async fn accept_within(&mut self, within: Duration) -> Option<(Sio, Value, String)> {
        tokio::time::timeout(within, self.accepted.recv())
            .await
            .ok()?
    }

    pub async fn accept(&mut self) -> (Sio, Value, String) {
        self.accept_within(Duration::from_secs(30))
            .await
            .expect("the core dialled the room")
    }
}

/// A room dialling the core's own rendezvous listener.
pub async fn dial(core: &Core, auth: Value) -> Result<Sio, Value> {
    let (ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{}/api/rendezvous/link/?EIO=4&transport=websocket",
        core.port
    ))
    .await
    .expect("the rendezvous listener accepts a WebSocket");
    Sio::client(ws, "/room", auth).await
}

// ---------------------------------------------------------------------------------------------------------------
// A connector's conversation on the v2 link.

/// What a connector says about its machine when it links.
pub fn identity() -> Value {
    json!({"host": "test-machine", "platform": "linux", "version": "test", "harnesses": ["fixture"]})
}

pub fn registration(client_ref: &str, thread: &str) -> Value {
    json!({"client_ref": client_ref, "harness": "fixture", "thread": thread, "title": "A conversation",
        "capabilities": {"deliver": "supported", "working": "supported"}, "inbound": {"ok": true}})
}

/// A connector on the v2 link with one conversation registered on `thread`; the binding the core gave it.
pub async fn v2_with_binding(core: &Core, client_ref: &str, thread: &str) -> (Sio, String) {
    let mut peer = connector_v2(core, identity()).await;
    assert_eq!(peer.event("connector.welcome").await.data["protocol"], 2);
    let binding = peer
        .call("binding.register", registration(client_ref, thread))
        .await
        .data;
    assert_eq!(binding["thread"], thread, "{binding}");
    let id = binding["binding_id"]
        .as_str()
        .expect("a binding id")
        .to_owned();
    (peer, id)
}

/// Acknowledges the next delivery as the conversation having taken it.
pub async fn accept_delivery(peer: &mut Sio) -> SioEvent {
    let delivery = peer.event("input.deliver").await;
    peer.answer(
        delivery.id.expect("a delivery asks for an answer"),
        json!({"status": "accepted", "detail": "accepted by the test connector"}),
    );
    delivery
}

/// A conversation's reply, as the connector publishes it on the v2 link.
pub async fn publish(
    peer: &Sio,
    binding: &str,
    session: &str,
    revision: u64,
    utterance: &str,
    text: &str,
) -> Value {
    peer.call(
        "speech.publish",
        json!({"event_id": format!("event-{utterance}"), "utterance_id": utterance, "binding_id": binding,
            "session_id": session, "revision": revision, "text": text, "language": "en"}),
    )
    .await
    .data
}

pub fn message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The models a test device reports in its hello, shaped as a client builds them from sidevoice-engine's `models()`.
pub fn device_models() -> Value {
    let builds = json!([
        {"id": "sherpa-onnx-int8", "backend": "sherpa-onnx", "accelerator": "cpu", "available": true},
        {"id": "transformers-js-q8", "backend": "transformers-js", "accelerator": "wasm", "available": true}]);
    let whisper = |id: &str| {
        json!({"id": id, "capabilities": ["stt"], "languages": ["es", "en", "fr", "it", "pt", "hi"],
               "installed": true, "builds": builds})
    };
    let voice = |id: &str, language: &str| json!({"id": id, "languages": [language]});
    json!({"version": 1, "models": [
        whisper("whisper-tiny"), whisper("whisper-base"), whisper("whisper-small"),
        {"id": "kokoro-82m-v1.0", "capabilities": ["tts"], "languages": ["en-US", "en-GB", "es"], "installed": true,
         "voices": [voice("ef_dora", "es"), voice("em_alex", "es"), voice("af_heart", "en-US"),
                    voice("af_bella", "en-US"), voice("bf_emma", "en-GB"), voice("ff_siwis", "fr"),
                    voice("if_sara", "it"), voice("pf_dora", "pt-BR"), voice("hf_alpha", "hi")],
         "builds": builds}]})
}
