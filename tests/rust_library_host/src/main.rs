use std::path::PathBuf;
use std::time::Duration;

use sidevoice_core::runtime::{self, Config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::oneshot;

async fn no_retained_tasks(before: usize) {
    let metrics = tokio::runtime::Handle::current().metrics();
    tokio::time::timeout(Duration::from_secs(2), async {
        while metrics.num_alive_tasks() != before {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Core tasks retired while host runtime stays alive");
}

#[tokio::main]
async fn main() {
    let data_dir = PathBuf::from(std::env::args().nth(1).expect("data directory"));
    let models =
        PathBuf::from(std::env::var_os("RUSTVANI_CACHE_DIR").expect("packaged model path"));
    assert!(models.join("silero.onnx").is_file());
    assert!(models.join("smart_turn_weights.bin.gz").is_file());
    let config = Config::from_args(&[
        "--data-dir".into(),
        data_dir.to_string_lossy().into_owned(),
        "--port".into(),
        "0".into(),
        "--launch-id".into(),
        "external-library-host".into(),
        "--idle-exit".into(),
        "0".into(),
    ])
    .expect("host configuration");
    let ready_file = config.ready_file.clone();
    let before = tokio::runtime::Handle::current().metrics().num_alive_tasks();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let service = tokio::spawn(runtime::run_with_shutdown(
        config,
        async move {
            let _ = shutdown_rx.await;
        },
        ready_tx,
    ));
    let ready = tokio::time::timeout(Duration::from_secs(10), ready_rx)
        .await
        .expect("startup deadline")
        .expect("startup report")
        .expect("Core startup");
    assert!(ready.port > 0);
    assert_eq!(ready.launch_id, "external-library-host");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ready_file).expect("readiness file"))
            .expect("readiness JSON");
    assert_eq!(record["port"].as_u64(), Some(u64::from(ready.port)));
    assert_eq!(record["launch_id"].as_str(), Some(ready.launch_id.as_str()));
    assert_eq!(record["socket"].as_str(), ready.socket.to_str());
    let mut local = UnixStream::connect(&ready.socket)
        .await
        .expect("local socket");
    local
        .write_all(
            b"GET /api/local/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("health request");
    let mut status = [0u8; 12];
    local
        .read_exact(&mut status)
        .await
        .expect("health response");
    assert_eq!(&status, b"HTTP/1.1 200");
    drop(local);
    shutdown_tx.send(()).expect("shutdown receiver");
    tokio::time::timeout(Duration::from_secs(15), service)
        .await
        .expect("shutdown deadline")
        .expect("host task")
        .expect("Core shutdown");
    assert!(!ready_file.exists());
    assert!(!ready.socket.exists());
    no_retained_tasks(before).await;

    // A directory in place of core.json fails only after both listeners are
    // serving. The host runtime remains alive to detect a retained room pump.
    let failed_dir = data_dir.join("failed-start");
    let blocked_ready = failed_dir.join("blocked-ready");
    std::fs::create_dir_all(&blocked_ready).expect("readiness collision");
    let failed_config = Config::from_args(&[
        "--data-dir".into(),
        failed_dir.to_string_lossy().into_owned(),
        "--ready-file".into(),
        blocked_ready.to_string_lossy().into_owned(),
        "--port".into(),
        "0".into(),
        "--idle-exit".into(),
        "0".into(),
    ])
    .expect("failure configuration");
    let failed_socket = failed_config.socket.clone();
    let (failed_tx, failed_rx) = oneshot::channel();
    let failed_service = tokio::spawn(runtime::run_with_shutdown(
        failed_config,
        std::future::pending(),
        failed_tx,
    ));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), failed_rx)
            .await
            .expect("failure report deadline")
            .expect("failure report")
            .expect_err("readiness write must fail")
            .step,
        "start"
    );
    assert_eq!(
        failed_service
            .await
            .expect("failed service task")
            .expect_err("failed service result")
            .step,
        "start"
    );
    assert!(!failed_socket.exists());
    no_retained_tasks(before).await;

    let held_dir = data_dir.join("held-connection");
    let held_config = Config::from_args(&[
        "--data-dir".into(),
        held_dir.to_string_lossy().into_owned(),
        "--port".into(),
        "0".into(),
        "--idle-exit".into(),
        "0".into(),
    ])
    .expect("held connection configuration");
    let held_ready_file = held_config.ready_file.clone();
    let (held_tx, held_rx) = oneshot::channel();
    let (held_shutdown_tx, held_shutdown_rx) = oneshot::channel();
    let held_service = tokio::spawn(runtime::run_with_shutdown(
        held_config,
        async move {
            let _ = held_shutdown_rx.await;
        },
        held_tx,
    ));
    let held_ready = held_rx.await.expect("held readiness report").expect("held startup");
    let mut held = UnixStream::connect(&held_ready.socket)
        .await
        .expect("held local connection");
    held.write_all(b"GET /api/local/health HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .expect("unfinished request");
    tokio::time::sleep(Duration::from_millis(100)).await;
    held_shutdown_tx.send(()).expect("held shutdown receiver");
    let result = tokio::time::timeout(Duration::from_secs(15), held_service)
        .await
        .expect("forced shutdown deadline")
        .expect("held service task");
    assert_eq!(result.expect_err("held request reaches graceful deadline").step, "run");
    let mut byte = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(2), held.read(&mut byte))
        .await
        .expect("held connection retired");
    assert!(closed.is_err() || closed.expect("held read") == 0);
    assert!(!held_ready_file.exists());
    assert!(!held_ready.socket.exists());
    assert!(TcpStream::connect(("127.0.0.1", held_ready.port)).await.is_err());
    no_retained_tasks(before).await;
    println!("external library host: normal, startup failure, held shutdown retired");
}
