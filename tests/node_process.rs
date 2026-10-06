//! The core as the connector starts it, driven as a real process.
//!
//! A core says where it listens in its ready file, serves its own user alone through a 0600
//! socket in a 0700 directory, leaves when nothing uses it, and — when it cannot start — says
//! why in `core-failure.json` and by its exit status what the service manager should do.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::{json, Value};

const CORE: &str = env!("CARGO_BIN_EXE_sidevoice-core-rust");

struct Core {
    child: Child,
}

impl Drop for Core {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Core {
    fn start(data: &Path, idle: &str, extra: &[&str]) -> Self {
        Self::start_in(None, data.to_str().unwrap(), idle, extra)
    }

    fn start_in(cwd: Option<&Path>, data: &str, idle: &str, extra: &[&str]) -> Self {
        let mut command = Command::new(CORE);
        command
            .args(["--port", "0", "--data-dir", data, "--idle-exit", idle])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        Self {
            child: command.spawn().expect("the core binary starts"),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn signal(&self, signal: libc::c_int) {
        assert_eq!(unsafe { libc::kill(self.pid() as libc::pid_t, signal) }, 0);
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn wait(&mut self, seconds: u64) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the core did not exit in time");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            stderr.read_to_string(&mut text).unwrap();
        }
        text
    }
}

/// What a start that runs to its end (a failed one) returns: status and stderr.
fn run_core(data: &str, extra: &[&str]) -> (ExitStatus, String) {
    let output = Command::new(CORE)
        .args([
            "--port",
            "0",
            "--data-dir",
            data,
            "--idle-exit",
            "0",
            "--launch-id",
            "launch-7",
        ])
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .expect("the core binary runs");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn ready(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn until<T>(seconds: u64, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_ready(data: &Path) -> Value {
    until(60, || ready(&data.join("core.json")))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn private_dir(path: &Path) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
}

/// One HTTP/1.1 exchange with `Connection: close`, over any stream.
fn exchange(mut stream: impl Read + Write, method: &str, path: &str, host: &str) -> (u16, Value) {
    let body = if method == "POST" { "{}" } else { "" };
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let (head, rest) = text.split_once("\r\n\r\n").expect("an HTTP answer");
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut decoded = String::new();
        let mut rest = rest;
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            decoded.push_str(&tail[..size]);
            rest = &tail[size + 2..];
        }
        decoded
    } else {
        rest.to_owned()
    };
    (
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    )
}

fn via_socket(socket: &Path, method: &str, path: &str) -> (u16, Value) {
    exchange(
        UnixStream::connect(socket).unwrap(),
        method,
        path,
        "localhost",
    )
}

fn over_tcp(port: u64, method: &str, path: &str) -> (u16, Value) {
    let stream = std::net::TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    exchange(stream, method, path, &format!("127.0.0.1:{port}"))
}

const LOCAL_PATHS: [(&str, &str); 4] = [
    ("GET", "/api/connectors/link/?EIO=4&transport=polling"),
    ("GET", "/api/local/health"),
    ("POST", "/api/device/local/pair"),
    ("DELETE", "/api/device/local"),
];

// --- A core that starts ----------------------------------------------------------------------

#[test]
fn the_ready_file_carries_the_link_and_leaves_with_the_process() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    let mut core = Core::start(&data, "0", &["--launch-id", "launch-1"]);
    let facts = wait_ready(&data);
    assert_eq!(
        mode(&data.join("core.json")),
        0o600,
        "it holds a credential"
    );
    assert_eq!(
        mode(&data),
        0o700,
        "a directory it created is this user's alone"
    );
    assert_eq!(facts["pid"], core.pid());
    let port = facts["port"].as_u64().unwrap();
    assert_eq!(facts["url"], format!("http://127.0.0.1:{port}"));
    assert_eq!(facts["protocol"], 2);
    assert_eq!(facts["connector_protocols"], json!([2, 3]));
    assert_eq!(
        (&facts["launch_id"], &facts["api"]),
        (&json!("launch-1"), &json!(1))
    );
    assert_eq!(facts["socket"], data.join("local.sock").to_str().unwrap());
    let socket = PathBuf::from(facts["socket"].as_str().unwrap());
    let meta = fs::symlink_metadata(&socket).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "the socket is this user's alone"
    );
    assert!(facts["connector_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert!(facts["token"]
        .as_str()
        .is_some_and(|token| !token.is_empty()));
    let (status, health) = via_socket(&socket, "GET", "/api/local/health");
    assert_eq!(status, 200);
    assert_eq!(
        (&health["launch_id"], &health["pid"]),
        (&json!("launch-1"), &json!(core.pid()))
    );
    // What only the socket serves does not exist over TCP.
    for (method, path) in LOCAL_PATHS {
        assert_eq!(
            over_tcp(port, method, path).0,
            404,
            "{method} {path} over TCP"
        );
    }
    core.signal(libc::SIGTERM);
    assert!(core.wait(20).success());
    assert!(
        !data.join("core.json").exists(),
        "the file leaves with the process"
    );
    assert!(!data.join("local.sock").exists(), "and so does the socket");
    // Started again, it keeps the same credential: a connector reconnecting needs no new one.
    let mut again = Core::start(&data, "0", &[]);
    let second = wait_ready(&data);
    assert_eq!(second["pid"], again.pid());
    let launch = second["launch_id"].as_str().unwrap();
    assert_eq!(
        uuid::Uuid::parse_str(launch).unwrap().to_string(),
        launch,
        "none was given: one is made up"
    );
    assert_eq!(
        (&second["connector_id"], &second["token"]),
        (&facts["connector_id"], &facts["token"])
    );
    again.signal(libc::SIGTERM);
    assert!(again.wait(20).success());
}

#[test]
fn a_manager_s_sigterm_or_a_sigint_is_a_clean_exit() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("core");
        let mut core = Core::start(&data, "0", &[]);
        wait_ready(&data);
        core.signal(signal);
        assert_eq!(core.wait(20).code(), Some(0), "signal {signal}");
        assert!(!data.join("core.json").exists());
        assert!(!data.join("local.sock").exists());
    }
}

#[test]
fn a_core_nothing_uses_leaves_on_its_own() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    let mut core = Core::start(&data, "1", &[]);
    wait_ready(&data);
    assert_eq!(core.wait(30).code(), Some(0));
    assert!(!data.join("core.json").exists());
    assert!(!data.join("local.sock").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_still_waiting_for_its_first_message_keeps_the_core_up_and_is_counted() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    let mut core = Core::start(&data, "2", &[]);
    let facts = wait_ready(&data);
    let socket = PathBuf::from(facts["socket"].as_str().unwrap());
    let (status, paired) = via_socket(&socket, "POST", "/api/device/local/pair");
    assert_eq!(status, 200, "{paired}");
    let token = paired["token"].as_str().unwrap();
    let mut request = "ws://localhost/api/presentation/ws"
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "sec-websocket-protocol",
        format!("sidevoice, sidevoice.token.{token}")
            .parse()
            .unwrap(),
    );
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    let (_, health) = via_socket(&socket, "GET", "/api/local/health");
    assert_eq!(health["calls"], 1, "before any hello");
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        core.running(),
        "a call waiting for its first message is a call"
    );
    ws.close(None).await.unwrap();
    drop(ws);
    assert_eq!(core.wait(30).code(), Some(0));
}

#[test]
fn a_start_clears_the_failure_report_a_previous_one_left() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    private_dir(&data);
    fs::write(
        data.join("core-failure.json"),
        r#"{"key": "identity.unreadable", "launch_id": "old"}"#,
    )
    .unwrap();
    let _core = Core::start(&data, "0", &[]);
    wait_ready(&data);
    assert!(!data.join("core-failure.json").exists());
}

#[test]
fn the_core_writes_its_own_private_log() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    let mut core = Core::start(&data, "0", &["--launch-id", "logged"]);
    wait_ready(&data);
    let log = root.path().join("core.log");
    let text = until(10, || {
        fs::read_to_string(&log)
            .ok()
            .filter(|text| text.contains("runtime.log_ready"))
    });
    assert_eq!(mode(&log), 0o600);
    assert!(text.lines().all(|line| {
        serde_json::from_str::<Value>(line).is_ok_and(|entry| entry["launch_id"] == "logged")
    }));
    core.signal(libc::SIGTERM);
    assert!(core.wait(20).success());
    assert!(fs::read_to_string(&log)
        .unwrap()
        .contains("runtime.log_stop"));
    let stderr = core.stderr();
    assert!(
        !stderr.contains("runtime.log_"),
        "the core's own lines go to its log, not the manager's: {stderr}"
    );
}

#[test]
fn a_socket_a_dead_core_left_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    private_dir(&data);
    // Bound, never listening, nobody behind it: what a SIGKILLed core leaves.
    drop(std::os::unix::net::UnixListener::bind(data.join("local.sock")).unwrap());
    assert!(data.join("local.sock").exists());
    let core = Core::start(&data, "0", &[]);
    let facts = wait_ready(&data);
    let (status, health) = via_socket(&data.join("local.sock"), "GET", "/api/local/health");
    assert_eq!((status, &health["pid"]), (200, &json!(core.pid())));
    assert_eq!(facts["pid"], core.pid());
}

#[test]
fn a_relative_data_directory_is_one_directory() {
    let root = tempfile::tempdir().unwrap();
    let mut core = Core::start_in(Some(root.path()), "relative-core", "0", &[]);
    // The core makes it absolute from its working directory, which the OS reports resolved
    // (macOS's temporary directory sits behind the `/var` -> `/private/var` link).
    let data = fs::canonicalize(root.path()).unwrap().join("relative-core");
    let facts = until(60, || {
        assert!(core.running(), "{}", core.stderr());
        ready(&data.join("core.json"))
    });
    assert_eq!(facts["socket"], data.join("local.sock").to_str().unwrap());
    let (status, _) = via_socket(&data.join("local.sock"), "GET", "/api/local/health");
    assert_eq!(status, 200);
}

#[test]
fn a_socket_named_through_an_alias_of_the_data_directory() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    std::os::unix::fs::symlink(root.path(), root.path().join("alias")).unwrap();
    let aliased = root.path().join("alias").join("core").join("local.sock");
    let mut core = Core::start(&data, "0", &["--socket", aliased.to_str().unwrap()]);
    let facts = until(60, || {
        assert!(core.running(), "{}", core.stderr());
        ready(&data.join("core.json"))
    });
    assert_eq!(facts["socket"], aliased.to_str().unwrap());
    assert!(
        fs::symlink_metadata(data.join("local.sock"))
            .unwrap()
            .file_type()
            .is_socket(),
        "one directory, two spellings"
    );
    let (_, health) = via_socket(&aliased, "GET", "/api/local/health");
    assert_eq!(health["pid"], core.pid());
}

#[test]
fn overlapping_starts_leave_exactly_one_core() {
    for _attempt in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("core");
        private_dir(&data);
        let mut rivals: Vec<_> = (0..2)
            .map(|n| Core::start(&data, "0", &["--launch-id", &format!("rival-{n}")]))
            .collect();
        let facts = wait_ready(&data);
        let loser = until(60, || {
            rivals
                .iter_mut()
                .position(|rival| rival.child.try_wait().unwrap().is_some())
        });
        let winner = 1 - loser;
        std::thread::sleep(Duration::from_secs(1));
        assert!(rivals[winner].running(), "the other one serves");
        assert_eq!(
            rivals[loser].wait(1).code(),
            Some(75),
            "tried again later by the manager"
        );
        assert_eq!(facts["pid"], rivals[winner].pid());
        let report: Value =
            serde_json::from_slice(&fs::read(data.join("core-failure.json")).unwrap()).unwrap();
        assert_eq!(
            (&report["key"], &report["step"]),
            (&json!("bind.core-running"), &json!("bind"))
        );
        assert_eq!(report["launch_id"], format!("rival-{loser}"));
        let (_, health) = via_socket(&data.join("local.sock"), "GET", "/api/local/health");
        assert_eq!(
            health["pid"],
            rivals[winner].pid(),
            "the socket is the survivor's"
        );
        rivals[winner].signal(libc::SIGTERM);
        assert!(rivals[winner].wait(20).success());
    }
}

// --- A start that fails ----------------------------------------------------------------------

/// The report a failed start left. Exit 0 unless said otherwise: a failure that would repeat is
/// not one for the service manager to restart.
fn failure(
    data: &Path,
    result: &(ExitStatus, String),
    step: &str,
    key: &str,
    status: i32,
) -> Value {
    assert_eq!(result.0.code(), Some(status), "{}", result.1);
    let path = data.join("core-failure.json");
    assert_eq!(mode(&path), 0o600);
    let report: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut keys: Vec<_> = report.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["at", "key", "launch_id", "message", "step"]);
    assert_eq!(
        (&report["launch_id"], &report["step"], &report["key"]),
        (&json!("launch-7"), &json!(step), &json!(key))
    );
    assert!(report["message"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));
    let at = report["at"].as_str().unwrap();
    assert!(
        at.contains('T') && at.ends_with('Z'),
        "an RFC 3339 time in UTC: {at}"
    );
    assert!(result.1.contains(key), "stderr says it too: {}", result.1);
    report
}

#[test]
fn an_identity_that_cannot_be_read() {
    for content in [
        &b"{\"private_key_pem\": \"not a key\"}"[..],
        b"\xff\xfe",
        b"{\"private_key_pem\": \"\xc3\x28\"}",
    ] {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("core");
        private_dir(&data);
        fs::write(data.join("node-identity.json"), content).unwrap();
        let result = run_core(data.to_str().unwrap(), &[]);
        failure(&data, &result, "identity", "identity.unreadable", 0);
        assert!(!data.join("core.json").exists(), "never says it is ready");
        assert_eq!(
            fs::read(data.join("node-identity.json")).unwrap(),
            content,
            "never replaced"
        );
    }
}

#[test]
fn any_other_failure_before_ready_is_start_failed() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    fs::write(root.path().join("a-file"), b"").unwrap();
    let ready_file = root.path().join("a-file").join("core.json");
    let result = run_core(
        data.to_str().unwrap(),
        &["--ready-file", ready_file.to_str().unwrap()],
    );
    failure(&data, &result, "start", "start.failed", 0);
    assert!(
        !data.join("local.sock").exists(),
        "nothing it bound is left"
    );
}

#[test]
fn a_failure_without_a_launch_id_reports_the_one_made_up() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    private_dir(&data);
    fs::write(data.join("node-identity.json"), b"not a key").unwrap();
    let output = Command::new(CORE)
        .args([
            "--port",
            "0",
            "--data-dir",
            data.to_str().unwrap(),
            "--idle-exit",
            "0",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let report: Value =
        serde_json::from_slice(&fs::read(data.join("core-failure.json")).unwrap()).unwrap();
    let launch = report["launch_id"].as_str().unwrap();
    assert_eq!(uuid::Uuid::parse_str(launch).unwrap().to_string(), launch);
    assert!(
        fs::read_to_string(root.path().join("core.log"))
            .unwrap()
            .contains(launch),
        "and the log says which"
    );
}

#[test]
fn a_directory_another_core_holds() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    private_dir(&data);
    let lock = fs::File::create(data.join("core.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    let result = run_core(data.to_str().unwrap(), &[]);
    failure(&data, &result, "bind", "bind.core-running", 75);
    assert_eq!(
        names(&data),
        ["core-failure.json", "core.lock"],
        "the lock is taken before anything is"
    );
}

#[test]
#[ignore = "known gap: refusal messages carry no parameters (they should name the socket path, the port and the directory mode)"]
fn a_refusal_names_what_it_refused() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    private_dir(&data);
    let lock = fs::File::create(data.join("core.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    let report = failure(
        &data,
        &run_core(data.to_str().unwrap(), &[]),
        "bind",
        "bind.core-running",
        75,
    );
    assert!(report["message"]
        .as_str()
        .unwrap()
        .contains(data.join("local.sock").to_str().unwrap()));
    drop(lock);
    let loose = root.path().join("loose");
    private_dir(&loose);
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o750)).unwrap();
    let report = failure(
        &loose,
        &run_core(loose.to_str().unwrap(), &[]),
        "directory",
        "identity.unsafe-directory",
        0,
    );
    assert!(report["message"].as_str().unwrap().contains("0750"));
}

#[test]
fn a_directory_other_users_may_enter() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    private_dir(&data);
    fs::set_permissions(&data, fs::Permissions::from_mode(0o750)).unwrap();
    let result = run_core(data.to_str().unwrap(), &[]);
    failure(&data, &result, "directory", "identity.unsafe-directory", 0);
    assert!(!data.join("local.sock").exists());
    assert!(
        !data.join("node-identity.json").exists(),
        "no secret is written into it"
    );
    assert_eq!(mode(&data), 0o750, "left as it was");
}

#[test]
fn a_data_directory_that_is_a_link_is_not_written_through() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = root.path().join("elsewhere");
    private_dir(&elsewhere);
    fs::write(elsewhere.join("core-failure.json"), "somebody else's").unwrap();
    let data = root.path().join("core");
    std::os::unix::fs::symlink(&elsewhere, &data).unwrap();
    let (status, stderr) = run_core(data.to_str().unwrap(), &[]);
    assert_eq!(status.code(), Some(0));
    assert!(stderr.contains("identity.unsafe-directory"), "{stderr}");
    assert_eq!(
        fs::read_to_string(elsewhere.join("core-failure.json")).unwrap(),
        "somebody else's",
        "untouched"
    );
    assert_eq!(
        names(&elsewhere),
        ["core-failure.json"],
        "nothing else either"
    );
}

#[test]
#[ignore = "known gap: a symlinked data directory spelled `core/`, `core/.` or `core//` is followed and written through"]
fn a_data_directory_that_is_a_link_is_not_written_through_however_it_is_spelled() {
    for suffix in ["/", "/.", "//"] {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        private_dir(&elsewhere);
        fs::write(elsewhere.join("core-failure.json"), "somebody else's").unwrap();
        let data = root.path().join("core");
        std::os::unix::fs::symlink(&elsewhere, &data).unwrap();
        let (status, stderr) = run_core(&format!("{}{suffix}", data.display()), &[]);
        assert_eq!(status.code(), Some(0), "core{suffix}");
        assert!(
            stderr.contains("identity.unsafe-directory"),
            "core{suffix}: {stderr}"
        );
        assert_eq!(names(&elsewhere), ["core-failure.json"], "core{suffix}");
    }
}

#[test]
fn a_port_in_use() {
    let root = tempfile::tempdir().unwrap();
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port().to_string();
    let data = root.path().join("core");
    let output = Command::new(CORE)
        .args([
            "--port",
            &port,
            "--data-dir",
            data.to_str().unwrap(),
            "--idle-exit",
            "0",
            "--launch-id",
            "launch-7",
        ])
        .output()
        .unwrap();
    let result = (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    );
    failure(&data, &result, "bind", "bind.port-in-use", 0);
    assert!(!data.join("core.json").exists());
    assert!(!data.join("local.sock").exists(), "no socket left behind");
}

#[test]
fn a_socket_a_live_core_serves_is_not_taken_from_it() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("core");
    let mut first = Core::start(&data, "0", &[]);
    assert_eq!(wait_ready(&data)["pid"], first.pid());
    let result = run_core(data.to_str().unwrap(), &[]);
    failure(&data, &result, "bind", "bind.core-running", 75);
    assert!(first.running(), "the first core is untouched");
    assert_eq!(
        ready(&data.join("core.json")).unwrap()["pid"],
        first.pid(),
        "and so is the file that says it is serving"
    );
    let (_, health) = via_socket(&data.join("local.sock"), "GET", "/api/local/health");
    assert_eq!(health["pid"], first.pid());
    first.signal(libc::SIGTERM);
    assert!(first.wait(20).success());
}
