use std::path::PathBuf;
use std::time::Duration;

use sidevoice_core::runtime::{self, Config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::oneshot;

#[tokio::main]
async fn main() {
    let data_dir = PathBuf::from(std::env::args().nth(1).expect("data directory"));
    let models = PathBuf::from(std::env::var_os("RUSTVANI_CACHE_DIR").expect("packaged model path"));
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
    let mut local = UnixStream::connect(&ready.socket).await.expect("local socket");
    local
        .write_all(b"GET /api/local/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("health request");
    let mut status = [0u8; 12];
    local.read_exact(&mut status).await.expect("health response");
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
    println!("external library host: ready, healthy, stopped");
}
