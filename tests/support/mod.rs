//! What every integration test drives the core with: the real binary as a process, HTTP over its TCP port or its
//! local socket, a browser's call socket, and a connector on the JSON-RPC v3 link, each written here against the
//! wire. Nothing from another repository, no Docker.
#![allow(dead_code)]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sidevoice_core::platform::fs::{Create, Files, Fs};
use sidevoice_core::platform::ipc::{Channel, Channels, Endpoint, Stream};
use sidevoice_core::platform::user::{User, UserId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const CORE: &str = env!("CARGO_BIN_EXE_sidevoice-core-rust");

/// How long a step may take before the test says which one did not happen.
pub const STEP: Duration = Duration::from_secs(10);

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

    /// Where the core says it listens for this machine's processes: the ready file's `endpoint`.
    pub fn endpoint(&self) -> Endpoint {
        self.ready["endpoint"]
            .as_str()
            .unwrap()
            .parse::<Endpoint>()
            .unwrap()
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

    #[cfg(unix)]
    pub fn signal(&self, signal: libc::c_int) {
        unsafe {
            libc::kill(self.pid() as libc::pid_t, signal);
        }
    }

    /// Asks the core to leave the way its manager does on this OS: SIGTERM on Unix; on Windows, which has no signal for
    /// a process without a console, a shutdown request over its local channel (decision "Graceful stop", 2026-10-22).
    pub fn terminate(&self) {
        #[cfg(unix)]
        self.signal(libc::SIGTERM);
        #[cfg(windows)]
        {
            let Some(endpoint) = self.ready["endpoint"]
                .as_str()
                .and_then(|endpoint| endpoint.parse::<Endpoint>().ok())
            else {
                return;
            };
            let request = Http::new(Target::Local(endpoint), "POST", "/api/local/shutdown");
            // Its own thread and runtime: this is called inside a test's runtime as well as outside one.
            // A core that is already gone answers nothing: `stop` and `Drop` then see it exit (or not).
            let _ = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(request.send())
            })
            .join();
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

    /// [`Core::terminate`], the way a service manager stops it; its exit status.
    pub fn stop(&mut self) -> i32 {
        self.terminate();
        self.exited(Duration::from_secs(15))
            .unwrap_or_else(|| panic!("the core ignored the request to leave: {}", self.log()))
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
        Http::new(Target::Local(self.endpoint()), method, path)
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

    /// A browser in a call: hello sent with this data (none when null), session assigned.
    pub async fn join(&self, token: &str, hello: Value) -> Browser {
        let mut browser = Browser::new(self.open_call(token).await);
        browser.focus_lookup = Some((self.port, token.to_owned()));
        let hello = if hello.is_null() { json!({}) } else { hello };
        browser.send("voice-hello", hello).await;
        let session = browser.frame("voice-session").await;
        browser.session = session["session_id"].as_str().unwrap().to_owned();
        browser.welcome = session;
        browser
    }

    /// A page coming back after a drop: a new socket whose hello asks for `session` with its token and
    /// the last frame it handled. The session it lands on, resumed or new.
    pub async fn resume(&self, token: &str, session: &str, resume: &str, last_seq: u64) -> Browser {
        let mut browser = Browser::new(self.open_call(token).await);
        browser.focus_lookup = Some((self.port, token.to_owned()));
        browser
            .send(
                "voice-hello",
                json!({"resume": {"session_id": session, "token": resume, "last_seq": last_seq}}),
            )
            .await;
        let welcome = browser.frame("voice-session").await;
        browser.session = welcome["session_id"].as_str().unwrap().to_owned();
        browser.welcome = welcome;
        browser
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.terminate();
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
    Local(Endpoint),
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
                Target::Local(endpoint) => {
                    exchange(Channels::connect(endpoint).await.unwrap(), &self).await
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

/// An event as a failure message shows it: nothing long.
fn summary(event: &Value) -> String {
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
    /// The highest `seq` among the frames read so far.
    pub last_seq: u64,
    /// Fail on an `error` frame met while waiting for any other kind, instead of passing over it.
    pub no_errors: bool,
    focus_lookup: Option<(u16, String)>,
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
            last_seq: 0,
            no_errors: false,
            focus_lookup: None,
        }
    }

    pub async fn send(&self, kind: &str, data: Value) {
        let _ = self.tx.send(Message::text(
            json!({"type": kind, "data": data}).to_string(),
        ));
    }

    pub fn send_raw(&self, message: Message) {
        let _ = self.tx.send(message);
    }

    /// Sends a client message with a fresh `client_msg_id` and waits for the room to take it; the id.
    pub async fn report(&mut self, kind: &str, mut data: Value) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        data["session_id"] = json!(self.session);
        data["client_msg_id"] = json!(id);
        self.send(kind, data).await;
        self.wait("voice-ack", STEP, |ack| ack["client_msg_id"] == id.as_str())
            .await;
        id
    }

    /// The person says `text`, as the call's voice module reports a turn: it names the turn and starts it, the room
    /// gives it a revision, and it ends with those words. The turn's revision.
    pub async fn say(&mut self, text: &str) -> u64 {
        let turn = uuid::Uuid::new_v4().to_string();
        let revision = self.start_turn(&turn).await;
        self.report(
            "voice-user-turn",
            json!({"phase": "finished", "turn_id": turn, "text": text}),
        )
        .await;
        revision
    }

    /// Starts turn `turn`: the revision the room gave it, from the started frame that names it.
    pub async fn start_turn(&mut self, turn: &str) -> u64 {
        let (port, token) = self.focus_lookup.as_ref().expect("focus lookup");
        let state = Http::new(
            Target::Tcp(*port),
            "GET",
            &format!("/api/presentation?session_id={}", self.session),
        )
        .token(token)
        .send()
        .await
        .json();
        let target = &state["call"]["target"];
        self.report(
            "voice-user-turn",
            json!({"phase": "started", "turn_id": turn,
                "thread_id": target["thread_id"], "binding_id": target["binding_id"]}),
        )
        .await;
        let started = self
            .wait("voice-user-turn", STEP, |data| {
                data["phase"] == "started" && data["turn_id"] == turn
            })
            .await;
        started["revision"].as_u64().expect("a revision")
    }

    /// Reports what became of reply `utterance` here.
    pub async fn playback(&mut self, utterance: &str, status: &str) {
        self.report(
            "voice-playback",
            json!({"utterance_id": utterance, "status": status}),
        )
        .await;
    }

    /// Plays a reply to its end, as the call's voice module reports it.
    pub async fn played(&mut self, utterance: &str) {
        self.playback(utterance, "playing").await;
        self.playback(utterance, "heard").await;
        let history_id = format!("{}:voice:{utterance}", self.session);
        self.hearing(&history_id, utterance, "heard", None).await;
    }

    /// The app's semantic acknowledgement, independent of the physical playback report.
    pub async fn hearing(
        &mut self,
        history_id: &str,
        utterance: &str,
        outcome: &str,
        heard_chars: Option<u64>,
    ) {
        let mut data = json!({"history_id":history_id,"utterance_id":utterance,
            "outcome":outcome});
        if let Some(extent) = heard_chars {
            data["heard_chars"] = json!(extent);
        }
        self.report("reply-hearing", data).await;
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
            if let Some(seq) = event["seq"].as_u64() {
                self.last_seq = self.last_seq.max(seq);
            }
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
            assert!(
                !(self.no_errors && event["type"] == "error"),
                "an error while waiting for {kind}: {}",
                summary(&event)
            );
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

    /// The network goes: the socket ends without a hang-up, so the core parks the call.
    pub async fn drop_link(self) {
        let _ = self.tx.send(Message::Close(None));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Hangs up, as the page does: a close with code 1000 ends the call instead of parking it.
    pub async fn close(self) {
        let _ = self.tx.send(Message::Close(Some(
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                reason: "".into(),
            },
        )));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Connector link v3: JSON-RPC 2.0 over a WebSocket on the local socket.

pub struct Rpc {
    tx: Writer,
    rx: SplitStream<WebSocketStream<Stream>>,
    seen: Vec<Value>,
    next_id: i64,
    pub close_code: Option<u16>,
}

impl Rpc {
    pub async fn open(core: &Core) -> Self {
        let stream = Channels::connect(&core.endpoint()).await.unwrap();
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

    /// A request to the core; its result.
    pub async fn call(&mut self, method: &str, params: Value) -> Value {
        self.request(method, params).await["result"].clone()
    }

    /// A notification to the core: no answer.
    pub fn emit(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The core's next request named `name`, waiting as long as a step.
    pub async fn event(&mut self, name: &str) -> Value {
        self.method(name, STEP).await
    }

    /// Ends the link from the connector's side.
    pub fn disconnect(self) {
        let _ = self.tx.send(Message::Close(None));
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
// A connector's conversation on the v3 link.

pub fn registration(client_ref: &str, thread: &str) -> Value {
    json!({"client_ref": client_ref, "harness": "fixture", "thread": thread, "title": "A conversation",
        "capabilities": {"deliver": "supported", "working": "supported"}, "inbound": {"ok": true}})
}

/// A connector on the v3 link with one conversation registered on `thread`; the binding the core gave it.
pub async fn connector_with_binding(core: &Core, client_ref: &str, thread: &str) -> (Rpc, String) {
    let mut peer = Rpc::hello(core).await;
    let binding = peer
        .call("binding.register", registration(client_ref, thread))
        .await;
    assert_eq!(binding["thread"], thread, "{binding}");
    let id = binding["binding_id"]
        .as_str()
        .expect("a binding id")
        .to_owned();
    (peer, id)
}

/// Acknowledges the next delivery as the conversation having taken it; what was delivered.
pub async fn accept_delivery(peer: &mut Rpc) -> Value {
    let delivery = peer.event("input.deliver").await;
    peer.answer(
        &delivery["id"],
        json!({"status": "accepted", "detail": "accepted by the test connector"}),
    );
    delivery["params"].clone()
}

/// A conversation's reply, as the connector publishes it.
pub async fn publish(
    peer: &mut Rpc,
    binding: &str,
    session: &str,
    revision: u64,
    epoch: u64,
    utterance: &str,
    text: &str,
) -> Value {
    peer.call(
        "speech.publish",
        json!({"event_id": format!("event-{utterance}"), "utterance_id": utterance, "binding_id": binding,
            "session_id": session, "revision": revision, "epoch":epoch, "text": text, "language": "en"}),
    )
    .await
}

pub fn message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------------------------------------------
// Private directories and files, the same on every OS.

/// A directory of this user's, open to nobody else, as the core makes its own.
pub fn private_dir(path: &Path) {
    Files::create_private_dir(path).unwrap();
}

/// Creates a new file at `path` as the core makes its own: private from its creation (`Files::open_private`), so what
/// a test plants is judged by its content, not refused as open to others.
pub fn create_private(path: &Path) -> fs::File {
    Files::open_private(path, Create::New).unwrap()
}

/// Writes a new private file at `path` (`create_private`).
pub fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    use std::io::Write;
    create_private(path).write_all(bytes.as_ref()).unwrap();
}

/// Whether what `path` names (a file or a directory) is this user's and open to nobody else.
pub fn is_private(path: &Path) -> bool {
    let file = Files::open_read(path).unwrap();
    UserId::current().owns_file(&file).unwrap() && !Files::open_to_others(&file).unwrap()
}

/// A temporary directory that is private to this user: `tempfile` makes its own with the process's umask, and the core
/// refuses a log whose parent (here, the data directory's) is not private. `path()` is the private directory in it.
pub struct PrivateTemp {
    _outer: tempfile::TempDir,
    dir: PathBuf,
}

pub fn private_tempdir() -> PrivateTemp {
    let outer = tempfile::tempdir().unwrap();
    let dir = outer.path().join("private");
    private_dir(&dir);
    PrivateTemp { _outer: outer, dir }
}

impl PrivateTemp {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}
